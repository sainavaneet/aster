//! Worker operations resolve inside the bound Orbit tab before touching agents.
use crate::api::schema::{
    AgentPromptParams, AgentStartParams, AgentTarget, OrbitAgentPromptParams,
    OrbitAgentStartParams, OrbitAgentStatusParams,
};
use crate::app::App;
use serde_json::{json, Value};

use super::orbit_lifecycle::WorkerRecord;
use super::responses::{encode_error, encode_error_body};
use crate::app::orbit_worker_policy::WorkerLaunch;

impl App {
    /// Whether `terminal_id` is a terminal inside the bound Orbit tab or a team
    /// role's tab right now.
    pub(crate) fn orbit_terminal_in_scope(&mut self, terminal_id: &str) -> bool {
        if self.resolve_orbit().is_err() {
            return false;
        }
        let Some(target) = self
            .resolve_terminal_target(terminal_id)
            .ok()
            .filter(|target| target.terminal_id == terminal_id)
        else {
            return false;
        };
        self.orbit_scope_contains(target.ws_idx, target.tab_idx)
    }

    /// A worker inside the bound Orbit tab or a role tab, as its public pane id.
    pub(super) fn orbit_worker_target(&mut self, id: &str, name: &str) -> Result<String, String> {
        self.resolve_orbit()
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let target = self
            .resolve_agent_target(name)
            .or_else(|original| {
                self.resolve_terminal_target(name)
                    .ok()
                    .filter(|target| {
                        target.terminal_id == name
                            && self.agent_info(target.ws_idx, target.pane_id).is_some()
                    })
                    .ok_or(original)
            })
            .map_err(|error| encode_error_body(id.into(), self.agent_target_error_body(error)))?;
        if !self.orbit_scope_contains(target.ws_idx, target.tab_idx) {
            return Err(encode_error(
                id.into(),
                "orbit_scope_violation",
                "Worker is outside the bound Orbit tab and the team role tabs",
            ));
        }
        self.public_pane_id(target.ws_idx, target.pane_id)
            .ok_or_else(|| encode_error(id.into(), "orbit_missing", "Worker pane is unavailable"))
    }

    /// Agents in the bound Orbit tab other than the lead.
    fn orbit_live_worker_count(&mut self) -> usize {
        let Ok((ws, tab)) = self.resolve_orbit() else {
            return 0;
        };
        self.state.workspaces[ws].tabs[tab]
            .layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane_id| self.agent_info(ws, pane_id))
            .filter(|agent| !self.orbit_is_lead_terminal(&agent.terminal_id))
            .count()
    }

    pub(super) fn handle_orbit_agent_start(
        &mut self,
        id: String,
        params: OrbitAgentStartParams,
    ) -> String {
        if params.worktree_branch().is_some() {
            return encode_error(
                id,
                "deferred_required",
                "Orbit worktree starts require deferred dispatch",
            );
        }
        let launch = match self.admit_orbit_worker(&id, &params) {
            Ok(launch) => launch,
            Err(response) => return response,
        };
        if params.reuse.unwrap_or(true) {
            if let Some(response) = self.reuse_orbit_worker(&id, &params, &launch) {
                return response;
            }
        }
        if let Err(response) = self.check_orbit_worker_limit(&id, 0) {
            return response;
        }
        let (ws, pane, pane_id) = match self.resolve_orbit_pane(params.pane_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let reserved = self.state.workspaces[ws]
            .terminal_id(pane)
            .is_some_and(|terminal_id| self.orbit_terminal_reserved(terminal_id.as_str()));
        if reserved {
            return encode_error(
                id,
                "orbit_pane_reserved",
                "Pane is reserved for a worker that is still starting; split or choose another Orbit pane",
            );
        }
        self.launch_orbit_worker(id, params.name, params.kind, pane_id, launch)
            .unwrap_or_else(|response| response)
    }

    /// Checks shared by every start; returns the launch model and permission
    /// mode after the allowlist and configured defaults.
    pub(super) fn admit_orbit_worker(
        &mut self,
        id: &str,
        params: &OrbitAgentStartParams,
    ) -> Result<WorkerLaunch, String> {
        if self.policy.persist_session && !self.orbit_review.ready() {
            return Err(encode_error(
                id.into(),
                "review_initializing",
                "Orbit baseline is initializing; retry shortly",
            ));
        }
        if params.name == crate::app::polaris::NAME {
            return Err(encode_error(
                id.into(),
                "invalid_params",
                "The polaris name is reserved for the lead agent",
            ));
        }
        self.ensure_orbit_team_loaded();
        if let Some((role, _)) = self.orbit_team.roster.member(&params.name) {
            return Err(encode_error(
                id.into(),
                "orbit_team_member",
                format!(
                    "{} is a member of team role {}; members start only in their role's tab, from orbit_team_define",
                    params.name, role.name
                ),
            ));
        }
        self.orbit_worker_policy
            .admit_launch(
                &params.kind,
                params.model.clone(),
                params.permission_mode.as_deref(),
            )
            .map_err(|(code, message)| encode_error(id.into(), code, message))
    }

    /// Live workers plus isolated starts still in flight, except `own` starts
    /// of the caller, must stay below `max_concurrent`.
    pub(super) fn check_orbit_worker_limit(&mut self, id: &str, own: usize) -> Result<(), String> {
        let limit = self.orbit_worker_policy.max_concurrent;
        let live = self.orbit_live_worker_count();
        let starting = self.orbit_starts.in_flight.saturating_sub(own);
        if limit > 0 && live + starting >= limit {
            let starting = match starting {
                0 => String::new(),
                count => format!(" and {count} starting"),
            };
            return Err(encode_error(
                id.into(),
                "orbit_worker_limit",
                format!(
                    "Orbit already has {live} live workers{starting} (max_concurrent = {limit}); \
                     stop one with orbit_agent_stop before starting another"
                ),
            ));
        }
        Ok(())
    }

    /// An idle matching worker instead of a new one; see `find_reusable_orbit_worker`.
    /// The permission mode matched is the effective one, so a configured default
    /// is never bypassed by reusing a worker started without it.
    fn reuse_orbit_worker(
        &mut self,
        id: &str,
        params: &OrbitAgentStartParams,
        launch: &WorkerLaunch,
    ) -> Option<String> {
        let (ws, pane, terminal_id) = self.find_reusable_orbit_worker(
            &params.kind,
            params.model.as_deref(),
            launch.permission_mode,
        )?;
        let agent = self.agent_info(ws, pane)?;
        tracing::info!(%terminal_id, requested = %params.name, "orbit worker reused");
        Some(
            json!({"id": id, "result": {
                "type": "agent_reused",
                "reused": true,
                "pane_id": agent.pane_id,
                "agent": agent,
                "worker": self.orbit_worker_json(&terminal_id),
            }})
            .to_string(),
        )
    }

    /// Starts the agent in an admitted Orbit pane, titles the pane, and records
    /// the worker's kind, model, and permission mode. `Err` carries a failed
    /// start's response.
    pub(super) fn launch_orbit_worker(
        &mut self,
        id: String,
        name: String,
        kind: String,
        pane_id: String,
        launch: WorkerLaunch,
    ) -> Result<String, String> {
        let mut args = crate::detect::parse_agent_label(&kind)
            .map(crate::app::orbit_notifications::report_allow_args)
            .unwrap_or_default();
        args.extend(launch.args());
        let response = self.handle_agent_start(
            id.clone(),
            AgentStartParams {
                name: name.clone(),
                kind: kind.clone(),
                pane_id: pane_id.clone(),
                args,
                timeout_ms: None,
            },
        );
        let mut value: Value = serde_json::from_str(&response).unwrap_or_default();
        let Some(terminal_id) = value["result"]["agent"]["terminal_id"]
            .as_str()
            .filter(|_| value.get("error").is_none())
            .map(str::to_owned)
        else {
            return Err(response);
        };
        self.handle_pane_rename(
            id,
            crate::api::schema::PaneRenameParams {
                pane_id: pane_id.clone(),
                label: Some(name.clone()),
            },
        );
        self.orbit_workers.insert(
            terminal_id.clone(),
            WorkerRecord::launched(name, kind, launch),
        );
        value["result"]["reused"] = json!(false);
        value["result"]["pane_id"] = json!(pane_id);
        value["result"]["worker"] = json!(self.orbit_worker_json(&terminal_id));
        Ok(value.to_string())
    }

    pub(super) fn handle_orbit_agent_status(
        &mut self,
        id: String,
        params: OrbitAgentStatusParams,
    ) -> String {
        if let Some(role) = params.role.as_deref() {
            return self.handle_orbit_role_status(id, role);
        }
        let target = match self.orbit_worker_target(&id, &params.name) {
            Ok(target) => target,
            Err(response) => return response,
        };
        let response = self.handle_agent_get(id, AgentTarget { target });
        self.annotate_orbit_agent_response(response)
    }

    /// Convert to the existing deferred submission after a server-atomic scope check.
    /// The regular prompt pipeline then checks foreground process and awaits PTY admission.
    pub(crate) fn orbit_agent_prompt_params(
        &mut self,
        id: &str,
        params: OrbitAgentPromptParams,
        for_assignment: bool,
    ) -> Result<AgentPromptParams, String> {
        if self.policy.persist_session && !self.orbit_review.ready() {
            return Err(encode_error(
                id.into(),
                "review_initializing",
                "Orbit baseline is initializing; retry shortly",
            ));
        }
        let name = match params.role.as_deref() {
            Some(role) => self
                .orbit_team_pick_member(role, params.kind.as_deref(), for_assignment)
                .map_err(|(code, message)| encode_error(id.into(), code, message))?,
            None if params.name.trim().is_empty() => {
                return Err(encode_error(
                    id.into(),
                    "invalid_params",
                    "name or role is required",
                ))
            }
            None => params.name,
        };
        let target = self.orbit_worker_target(id, &name)?;
        let resolved = self
            .resolve_agent_target(&target)
            .map_err(|error| encode_error_body(id.into(), self.agent_target_error_body(error)))?;
        let Some(terminal_id) =
            self.state.workspaces[resolved.ws_idx].terminal_id(resolved.pane_id)
        else {
            return Err(encode_error(
                id.into(),
                "orbit_missing",
                "Worker terminal is unavailable",
            ));
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return Err(encode_error(
                id.into(),
                "orbit_missing",
                "Worker terminal is unavailable",
            ));
        };
        if terminal.state != crate::detect::AgentState::Idle {
            return Err(encode_error(
                id.into(),
                "orbit_worker_not_idle",
                "Worker is busy, blocked, or not yet ready; inspect its status before prompting",
            ));
        }
        Ok(AgentPromptParams {
            target,
            text: params.prompt,
            wait: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    fn app_with_workers() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut orbit = Workspace::test_new("orbit-workers");
        orbit.custom_name = Some("Polaris".into());
        orbit.tabs[0].set_custom_name("Orbit".into());
        app.state.workspaces.push(orbit);
        app.state.workspaces.push(Workspace::test_new("outside"));
        app.state.ensure_test_terminals();
        app.state.switch_workspace(1);
        app.prepare_orbit_startup();
        for (index, name) in [(0, "inside"), (1, "outside")] {
            let workspace = &app.state.workspaces[index];
            let id = workspace
                .terminal_id(workspace.tabs[0].root_pane)
                .unwrap()
                .clone();
            let terminal = app.state.terminals.get_mut(&id).unwrap();
            terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
            terminal.set_agent_name(name.into());
        }
        app
    }

    #[test]
    fn orbit_worker_status_rejects_outside_agent_and_preserves_focus() {
        let mut app = app_with_workers();
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_status(
            "status".into(),
            OrbitAgentStatusParams {
                name: "outside".into(),
                role: None,
            },
        ))
        .unwrap();
        assert_eq!(response["error"]["code"], "orbit_scope_violation");
        assert_eq!(app.state.active, Some(1));
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_status(
            "status".into(),
            OrbitAgentStatusParams {
                name: "inside".into(),
                role: None,
            },
        ))
        .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(app.state.active, Some(1));
    }

    #[test]
    fn orbit_worker_stable_terminal_id_survives_rename() {
        let mut app = app_with_workers();
        let resolved = app.resolve_agent_target("inside").unwrap();
        let terminal_id = resolved.terminal_id.clone();
        app.state
            .terminals
            .get_mut(terminal_id.as_str())
            .unwrap()
            .set_agent_name("renamed".into());
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_status(
            "status".into(),
            OrbitAgentStatusParams {
                name: terminal_id.clone(),
                role: None,
            },
        ))
        .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["agent"]["terminal_id"], terminal_id);
        assert_eq!(response["result"]["agent"]["name"], "renamed");
    }

    #[test]
    fn orbit_worker_prompt_checks_scope_and_busy_state_before_submission() {
        let mut app = app_with_workers();
        let response = app
            .orbit_agent_prompt_params(
                "prompt",
                OrbitAgentPromptParams {
                    name: "outside".into(),
                    prompt: "inspect".into(),
                    role: None,
                    kind: None,
                },
                false,
            )
            .unwrap_err();
        assert!(response.contains("orbit_scope_violation"));
        let target = app.resolve_agent_target("inside").unwrap();
        let id = app.state.workspaces[target.ws_idx]
            .terminal_id(target.pane_id)
            .unwrap()
            .clone();
        app.state
            .terminals
            .get_mut(&id)
            .unwrap()
            .set_detected_state(Some(Agent::Claude), AgentState::Working);
        let response = app
            .orbit_agent_prompt_params(
                "prompt",
                OrbitAgentPromptParams {
                    name: "inside".into(),
                    prompt: "inspect".into(),
                    role: None,
                    kind: None,
                },
                false,
            )
            .unwrap_err();
        assert!(response.contains("orbit_worker_not_idle"));
    }

    #[test]
    fn orbit_worker_prompt_resolves_to_explicit_bound_pane() {
        let mut app = app_with_workers();
        let params = app
            .orbit_agent_prompt_params(
                "prompt",
                OrbitAgentPromptParams {
                    name: "inside".into(),
                    prompt: "inspect".into(),
                    role: None,
                    kind: None,
                },
                false,
            )
            .unwrap();
        assert_eq!(params.text, "inspect");
        assert_eq!(
            params.target,
            app.public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
                .unwrap()
        );
        assert!(params.wait.is_none());
    }

    #[tokio::test]
    async fn orbit_worker_start_queues_safe_model_launch_without_focus_change() {
        let mut app = app_with_workers();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        app.state.terminals.insert(
            id.clone(),
            crate::terminal::TerminalState::new(id.clone(), "/".into()),
        );
        let (runtime, mut queue) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.terminal_runtimes.insert(id.clone(), runtime);
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_start(
            "start".into(),
            OrbitAgentStartParams {
                name: "reviewer".into(),
                kind: "claude".into(),
                pane_id: None,
                model: Some("fable".into()),
                reuse: None,
                worktree: None,
                permission_mode: None,
            },
        ))
        .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(
            app.state.terminals[&id].manual_label.as_deref(),
            Some("reviewer")
        );
        let bytes = queue.try_recv().expect("launch queued in Orbit");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("claude"));
        assert!(text.contains("fable"));
        assert!(!text.contains("skip-permissions"));
        assert_eq!(app.state.active, Some(1));
    }

    #[tokio::test]
    async fn orbit_worker_prompt_uses_deferred_acknowledgement() {
        let mut app = app_with_workers();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let (runtime, mut queue) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.state.insert_test_runtime(pane, runtime);
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: "prompt".into(),
                method: crate::api::schema::Method::OrbitAgentPrompt(OrbitAgentPromptParams {
                    name: "inside".into(),
                    prompt: "inspect only".into(),
                    role: None,
                    kind: None,
                }),
            },
            tx
        ));
        let response = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("submission acknowledged");
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert!(String::from_utf8_lossy(&queue.try_recv().unwrap()).contains("inspect only"));
        assert_eq!(app.state.active, Some(1));
    }

    fn start_params(kind: &str, model: Option<&str>) -> OrbitAgentStartParams {
        OrbitAgentStartParams {
            name: "reviewer".into(),
            kind: kind.into(),
            pane_id: None,
            model: model.map(Into::into),
            reuse: None,
            worktree: None,
            permission_mode: None,
        }
    }

    fn workers_config(toml: &str) -> crate::config::PolarisWorkersConfig {
        toml::from_str(toml).unwrap()
    }

    /// Replaces the Orbit root pane with an empty shell ready to launch into.
    fn launchable_root(app: &mut App) -> tokio::sync::mpsc::Receiver<bytes::Bytes> {
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        app.state.terminals.insert(
            id.clone(),
            crate::terminal::TerminalState::new(id.clone(), "/".into()),
        );
        let (runtime, queue) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.terminal_runtimes.insert(id, runtime);
        queue
    }

    fn start(app: &mut App, params: OrbitAgentStartParams) -> serde_json::Value {
        serde_json::from_str(&app.handle_orbit_agent_start("start".into(), params)).unwrap()
    }

    #[test]
    fn orbit_worker_start_rejects_kinds_and_models_outside_the_allowlist() {
        let mut app = app_with_workers();
        app.set_orbit_worker_policy(&workers_config(
            "kinds = [\"claude\"]\n[models]\nclaude = [\"fable\", \"opus\"]\n",
        ));
        let response = start(&mut app, start_params("codex", None));
        assert_eq!(response["error"]["code"], "orbit_worker_not_allowed");
        assert_eq!(
            response["error"]["message"],
            "Worker kind \"codex\" is not allowed; allowed kinds: claude"
        );
        let response = start(&mut app, start_params("claude", Some("haiku")));
        assert_eq!(response["error"]["code"], "orbit_worker_not_allowed");
        assert_eq!(
            response["error"]["message"],
            "Model \"haiku\" is not allowed for claude workers; allowed models: fable, opus"
        );
        app.set_orbit_worker_policy(&workers_config("kinds = [\"claude\", \"gemini\"]\n"));
        let response = start(&mut app, start_params("gemini", Some("pro")));
        assert_eq!(
            response["error"]["message"],
            "A model override currently supports Claude and Codex workers"
        );
    }

    #[tokio::test]
    async fn orbit_worker_start_accepts_allowed_model() {
        let mut app = app_with_workers();
        app.set_orbit_worker_policy(&workers_config(
            "kinds = [\"claude\"]\n[models]\nclaude = [\"fable\", \"opus\"]\n",
        ));
        let mut queue = launchable_root(&mut app);
        let response = start(&mut app, start_params("claude", Some("opus")));
        assert!(response.get("error").is_none(), "{response}");
        let text = String::from_utf8_lossy(&queue.try_recv().unwrap()).into_owned();
        assert!(text.contains("--model") && text.contains("opus"), "{text}");
    }

    #[tokio::test]
    async fn orbit_worker_start_applies_the_default_model() {
        let mut app = app_with_workers();
        app.set_orbit_worker_policy(&workers_config(
            "kinds = [\"claude\"]\n[defaults]\nclaude = \"sonnet\"\n",
        ));
        let mut queue = launchable_root(&mut app);
        let response = start(&mut app, start_params("claude", None));
        assert!(response.get("error").is_none(), "{response}");
        let text = String::from_utf8_lossy(&queue.try_recv().unwrap()).into_owned();
        assert!(
            text.contains("--model") && text.contains("sonnet"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn orbit_claude_workers_pre_allow_only_their_report_command() {
        let claude = mode_params("claude", "manual");
        for (params, allowed) in [(claude, true), (start_params("codex", None), false)] {
            let mut app = app_with_workers();
            let mut queue = launchable_root(&mut app);
            let response = start(&mut app, params);
            assert!(response.get("error").is_none(), "{response}");
            let text = String::from_utf8_lossy(&queue.try_recv().unwrap()).into_owned();
            assert_eq!(text.contains("--allowedTools"), allowed, "{text}");
            if allowed {
                assert_eq!(text.matches("--allowedTools").count(), 1, "{text}");
                assert!(text.contains(" orbit agent-report:*)"), "{text}");
                assert!(text.contains("--permission-mode manual"), "{text}");
            }
        }
    }

    fn mode_params(kind: &str, mode: &str) -> OrbitAgentStartParams {
        OrbitAgentStartParams {
            permission_mode: Some(mode.into()),
            ..start_params(kind, None)
        }
    }

    #[tokio::test]
    async fn orbit_worker_start_maps_permission_mode_into_launch_args_and_record() {
        let mut app = app_with_workers();
        let mut queue = launchable_root(&mut app);
        let response = start(&mut app, mode_params("claude", "manual"));
        assert!(response.get("error").is_none(), "{response}");
        let text = String::from_utf8_lossy(&queue.try_recv().unwrap()).into_owned();
        assert!(text.contains("--permission-mode manual"), "{text}");
        assert!(!text.contains("bypass"), "{text}");
        assert_eq!(response["result"]["worker"]["permission_mode"], "manual");
        let terminal_id = response["result"]["agent"]["terminal_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let status: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_status(
            "status".into(),
            OrbitAgentStatusParams {
                name: terminal_id,
                role: None,
            },
        ))
        .unwrap();
        assert_eq!(status["result"]["worker"]["permission_mode"], "manual");
    }

    #[tokio::test]
    async fn orbit_worker_start_applies_the_configured_permission_mode() {
        let mut app = app_with_workers();
        app.set_orbit_worker_policy(&workers_config("permission_mode = \"plan\"\n"));
        let mut queue = launchable_root(&mut app);
        let response = start(&mut app, start_params("claude", None));
        assert!(response.get("error").is_none(), "{response}");
        let text = String::from_utf8_lossy(&queue.try_recv().unwrap()).into_owned();
        assert!(text.contains("--permission-mode plan"), "{text}");
        assert_eq!(response["result"]["worker"]["permission_mode"], "plan");
    }

    #[test]
    fn orbit_worker_start_refuses_bypass_and_non_claude_permission_modes() {
        let mut app = app_with_workers();
        let response = start(&mut app, mode_params("claude", "bypassPermissions"));
        assert_eq!(response["error"]["code"], "orbit_permission_mode_refused");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("never allowed"));
        let response = start(&mut app, mode_params("codex", "manual"));
        assert_eq!(
            response["error"]["code"],
            "orbit_permission_mode_unsupported"
        );
        let response = start(&mut app, mode_params("claude", "ask"));
        assert_eq!(response["error"]["code"], "invalid_params");
    }

    #[test]
    fn orbit_worker_reuse_matches_the_effective_permission_mode() {
        use super::super::orbit_lifecycle::tests::{add_worker, orbit_app};
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        // A worker started without a mode is not reused for a manual request.
        let response = start(&mut app, mode_params("claude", "manual"));
        assert_ne!(response["result"]["reused"], true, "{response}");
        app.orbit_workers.get_mut(&worker).unwrap().permission_mode = Some("manual");
        let response = start(&mut app, mode_params("claude", "manual"));
        assert_eq!(response["result"]["reused"], true, "{response}");
        assert_eq!(response["result"]["worker"]["permission_mode"], "manual");
        let response = start(&mut app, mode_params("claude", "plan"));
        assert_ne!(response["result"]["reused"], true, "{response}");
        // No mode requested or configured: any mode matches.
        let response = start(&mut app, start_params("claude", None));
        assert_eq!(response["result"]["reused"], true, "{response}");
        // A configured default counts as requested.
        app.set_orbit_worker_policy(&workers_config("permission_mode = \"auto\"\n"));
        let response = start(&mut app, start_params("claude", None));
        assert_ne!(response["result"]["reused"], true, "{response}");
    }

    #[test]
    fn orbit_worker_start_enforces_max_concurrent_workers() {
        let mut app = app_with_workers();
        // The "inside" agent is already a live Orbit worker.
        assert_eq!(app.orbit_live_worker_count(), 1);
        app.set_orbit_worker_policy(&workers_config("max_concurrent = 1\n"));
        let response = start(&mut app, start_params("claude", None));
        assert_eq!(response["error"]["code"], "orbit_worker_limit");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_concurrent = 1"));
        // The lead never counts against the limit.
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        app.bind_orbit_lead(id.to_string());
        assert_eq!(app.orbit_live_worker_count(), 0);
        app.set_orbit_worker_policy(&workers_config("max_concurrent = 0\n"));
        let response = start(&mut app, start_params("claude", None));
        assert_ne!(response["error"]["code"], "orbit_worker_limit");
    }

    #[test]
    fn orbit_activity_lists_allowed_worker_kinds() {
        let mut app = app_with_workers();
        app.set_orbit_worker_policy(&workers_config(
            "kinds = [\"codex\"]\n[defaults]\ncodex = \"gpt-5-codex\"\n",
        ));
        let response: serde_json::Value =
            serde_json::from_str(&app.handle_orbit_activity("activity".into())).unwrap();
        let kinds = response["result"]["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0]["kind"], "codex");
        assert_eq!(kinds[0]["executable"], "codex");
        assert!(kinds[0]["installed"].is_boolean());
        assert_eq!(kinds[0]["models"], serde_json::json!([]));
        assert_eq!(kinds[0]["default_model"], "gpt-5-codex");
    }

    #[test]
    fn orbit_worker_start_rejects_foreign_pane_before_launch() {
        let mut app = app_with_workers();
        let pane_id = app.public_pane_id(1, app.state.workspaces[1].tabs[0].root_pane);
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_agent_start(
            "start".into(),
            OrbitAgentStartParams {
                name: "worker".into(),
                kind: "claude".into(),
                pane_id,
                model: None,
                reuse: None,
                worktree: None,
                permission_mode: None,
            },
        ))
        .unwrap();
        assert_eq!(response["error"]["code"], "orbit_scope_violation");
    }

    #[test]
    fn orbit_worker_start_reuses_idle_worker_of_same_kind_and_model() {
        use super::super::orbit_lifecycle::tests::{add_worker, orbit_app, orbit_panes};
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", Some("opus"));
        let (_, pane_id, _) = orbit_panes(&mut app)[1].clone();
        let response = start(&mut app, start_params("claude", Some("opus")));
        assert_eq!(response["result"]["reused"], true, "{response}");
        assert_eq!(response["result"]["pane_id"], pane_id);
        assert_eq!(response["result"]["agent"]["terminal_id"], worker);
        assert_eq!(response["result"]["worker"]["model"], "opus");
        // The limit does not block reuse.
        app.set_orbit_worker_policy(&workers_config("max_concurrent = 1\n"));
        let response = start(&mut app, start_params("claude", None));
        assert_eq!(response["result"]["reused"], true, "{response}");
        for params in [
            start_params("claude", Some("sonnet")),
            start_params("codex", None),
            OrbitAgentStartParams {
                reuse: Some(false),
                ..start_params("claude", Some("opus"))
            },
        ] {
            let response = start(&mut app, params);
            assert_ne!(response["result"]["reused"], true, "{response}");
        }
        assert_eq!(app.state.active, Some(1));
    }
}
