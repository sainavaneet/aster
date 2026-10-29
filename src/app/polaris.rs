//! One lead pane per interactive server startup. Session restore owns resumed agents.
use std::time::{Duration, Instant};

use super::{agents::AgentStartError, App, Mode};
use crate::{api::schema::AgentStartParams, config::PolarisConfig};

pub(super) const NAME: &str = "polaris";
const LABEL: &str = "Polaris";
const RETRY_INTERVAL: Duration = Duration::from_millis(250);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const LEAD_PROMPT: &str = "You are Polaris, Aster's lead: the CEO of a team of agent employees. You hold the overview and route work; you never do the work yourself. Wait for the user's task, then route each request to the responsible team role and only that role: call orbit_team_list to know which role owns which area and who its members are, and use orbit_agent_assign with role (optionally kind) so an idle member of that role takes it. Team members live in their role's tab, never exit, and are resumed with their own conversation after a restart; keep them alive and never stop them (orbit_agent_stop refuses members; retire a role only with orbit_team_remove, define or update roles with orbit_team_define when the user asks). Pinned roles come from the user's config and cannot be removed by Polaris. The user may also talk to a member directly in its role tab. Scratch workers in the Orbit tab are for one-off work only, when no role owns the request. Use Orbit tools, preserving focus and approvals. Use orbit_service_start for servers and watchers that must keep running, orbit_service_list to inspect them, and orbit_service_stop to stop them. Use orbit_exec and orbit_job_status for commands that finish. For a scratch worker, split before starting it; pick each worker's kind and model per task from orbit_activity.kinds (for example Codex for reviews, Claude for edits), staying within the configured allowlist; wait until idle, then use orbit_agent_assign and continue other work without polling. Worker reports arrive automatically as prompts, sometimes several in one prompt; handle each marked report. Reports carry tests_run, checks, and changed_files; run the tests yourself before accepting edits when tests_run is false or missing, or assign that check to the role that owns testing. Aster marks notices it generates for lost, unresponsive, or timed-out workers as origin server; treat them as Aster's, not the worker's. When a worker is blocked on an approval prompt, ask the user to answer in its pane; use orbit_agent_approve or orbit_agent_deny only when Aster allows it. Aster's decision layer may answer an approval prompt itself; the notice then ends with a Decision line saying what it did and why. orbit_route suggests an owner role for a request; always confirm before assigning. Start a worker with permission_mode manual when it should ask before acting; a blocked worker with no assignment is reported the same way as a blocked assignment. Reuse an idle worker before starting another (orbit_agent_start reuses a matching idle worker by default); start a worker with worktree true when its edits could conflict with other work. Keep workers alive between tasks so they retain context for follow-ups; stop a scratch worker with orbit_agent_stop only when its area of work is finished or a pane or worker slot is needed, and close panes listed in orbit_activity.stale_panes with orbit_pane_close. Arrange Orbit with orbit_pane_resize, orbit_pane_zoom, and orbit_pane_close, not raw pane calls. A role with several members may relay one task across them: a member hands its part to another member of the same role with a handoff note, and the task's steps appear in orbit_task_status and orbit_activity as step N/M. Do not reassign or cancel a task that is mid-relay; you receive one report when the last member finishes. For independent parts of one request that touch different files, give orbit_agent_assign parallel (a list of {member_or_kind, prompt, files}) instead of a worker: each part runs in a new agent in its own checkout, overlapping parts run one after another, the integrator (the role's configured integrator, else the reviewer role's first member, else the role's first member; pass role to choose the team) applies their patches to the main tree with git apply --3way in order, resolves any conflict, runs the full tests, and you get that integrator's one report. An integration you do not want is undone with git apply -R --index on the saved patches in reverse order. Inspect with orbit_activity, orbit_team_list, orbit_task_status, or orbit_read; abandon a task with orbit_task_cancel.";

pub(super) struct PendingPolarisStart {
    params: AgentStartParams,
    next_attempt: Instant,
    deadline: Instant,
}

fn launch_params(config: &PolarisConfig, pane_id: String) -> Result<AgentStartParams, String> {
    if config.provider != "claude" {
        return Err(format!(
            "Polaris currently supports Claude; configured provider is {}",
            config.provider
        ));
    }
    if config.model.trim().is_empty() || config.model.chars().any(char::is_control) {
        return Err("Polaris requires a non-empty Claude model without control characters".into());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mcp = serde_json::json!({"mcpServers": {"orbit": {
        "command": executable, "args": ["orbit-mcp"],
        "env": {"ASTER_SOCKET_PATH": crate::api::socket_path()}
    }}})
    .to_string();
    Ok(AgentStartParams {
        name: NAME.into(),
        kind: config.provider.clone(),
        pane_id,
        args: vec![
            "--model".into(),
            config.model.clone(),
            "--append-system-prompt".into(),
            LEAD_PROMPT.into(),
            "--mcp-config".into(),
            mcp,
        ],
        timeout_ms: None,
    })
}

impl App {
    pub(crate) fn prepare_polaris_startup(&mut self, config: &PolarisConfig, fresh_session: bool) {
        if self.polaris_startup_prepared {
            return;
        }
        self.polaris_startup_prepared = true;
        if !config.enabled {
            return;
        }
        // Validate before allocating a pane or changing focus.
        let mut params = match launch_params(config, String::new()) {
            Ok(params) => params,
            Err(message) => {
                self.polaris_startup_error(message);
                return;
            }
        };
        // Persisted workspace identity prevents duplicates across server restarts;
        // native agent resume retains its own authority over restored terminals.
        let existing = self.state.workspaces.iter().position(|workspace| {
            workspace.custom_name.as_deref() == Some(LABEL)
                || workspace.tabs.iter().any(|tab| {
                    tab.panes.values().any(|pane| {
                        self.state
                            .terminals
                            .get(&pane.attached_terminal_id)
                            .is_some_and(|terminal| terminal.agent_name.as_deref() == Some(NAME))
                    })
                })
        });
        let index = if let Some(index) = existing {
            index
        } else if fresh_session && self.state.workspaces.len() == 1 {
            0
        } else {
            let cwd = self
                .state
                .active
                .and_then(|index| self.focused_pane_cwd_in_workspace(index))
                .unwrap_or_else(|| self.resolve_new_terminal_cwd(None));
            match self.create_workspace_with_options(cwd, true) {
                Ok(index) => index,
                Err(error) => {
                    self.polaris_startup_error(format!("Could not open Polaris: {error}"));
                    return;
                }
            }
        };
        self.state.switch_workspace(index);
        self.state.mode = Mode::Terminal;
        if existing.is_some() {
            let workspace = &self.state.workspaces[index];
            let target = workspace
                .tabs
                .iter()
                .find_map(|tab| {
                    tab.panes.iter().find_map(|(pane_id, pane)| {
                        self.state
                            .terminals
                            .get(&pane.attached_terminal_id)
                            .filter(|terminal| terminal.agent_name.as_deref() == Some(NAME))
                            .map(|_| *pane_id)
                    })
                })
                .or_else(|| workspace.tabs.first().map(|tab| tab.root_pane));
            if let Some(target) = target {
                self.state.focus_pane_in_workspace(index, target);
            }
        }
        if existing.is_some()
            && self.state.workspaces[index].tabs.iter().any(|tab| {
                tab.panes.values().any(|pane| {
                    self.state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .is_some_and(|terminal| {
                            terminal.is_agent_terminal()
                                || terminal.managed_agent_kind().is_some()
                                || terminal.pending_agent_resume_plan.is_some()
                        })
                })
            })
        {
            return;
        }
        let workspace = &mut self.state.workspaces[index];
        workspace.custom_name = Some(LABEL.into());
        if let Some(tab) = workspace.active_tab_mut() {
            tab.set_custom_name(LABEL.into());
        }
        let Some(pane_id) = workspace
            .focused_pane_id()
            .and_then(|pane| self.public_pane_id(index, pane))
        else {
            self.polaris_startup_error("Polaris workspace has no launch pane".into());
            return;
        };
        params.pane_id = pane_id;
        self.state.switch_workspace(index);
        self.state.mode = Mode::Terminal;
        self.schedule_session_save();
        let now = Instant::now();
        self.pending_polaris_start = Some(PendingPolarisStart {
            params,
            next_attempt: now + RETRY_INTERVAL,
            deadline: now + STARTUP_TIMEOUT,
        });
    }

    pub(crate) fn start_pending_polaris(&mut self, now: Instant) -> bool {
        let Some(pending) = self.pending_polaris_start.as_ref() else {
            return false;
        };
        if now < pending.next_attempt {
            return false;
        }
        let Some(mut pending) = self.pending_polaris_start.take() else {
            return false;
        };
        match self.start_agent(pending.params.clone()) {
            Ok((agent, _)) => {
                self.polaris_error = None;
                self.bind_orbit_lead(agent.terminal_id);
                true
            }
            Err(AgentStartError::TargetBusy(_) | AgentStartError::ShellNotReady(_))
                if now < pending.deadline =>
            {
                pending.next_attempt = now + RETRY_INTERVAL;
                self.pending_polaris_start = Some(pending);
                false
            }
            Err(error) => {
                let error = self.agent_start_error_body(error);
                self.polaris_startup_error(format!("Polaris could not start: {}", error.message));
                true
            }
        }
    }

    fn polaris_startup_error(&mut self, message: String) {
        tracing::warn!("{message}");
        self.polaris_error = Some(message.clone());
        self.state.config_diagnostic = Some(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            super::super::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }
    #[tokio::test]
    async fn orbit_startup_creates_one_work_tab_and_preserves_focus() {
        let mut app = app();
        let mut workspace = crate::workspace::Workspace::test_new("polaris");
        workspace.custom_name = Some(LABEL.into());
        workspace.tabs[0].set_custom_name(LABEL.into());
        app.state.workspaces.push(workspace);
        app.state.switch_workspace(0);
        let original_pane = app.state.workspaces[0].focused_pane_id();
        app.prepare_orbit_startup();
        app.prepare_orbit_startup();
        let workspace = &app.state.workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(workspace.tabs[1].custom_name.as_deref(), Some("Orbit"));
        assert_eq!(workspace.focused_pane_id(), original_pane);
    }

    #[test]
    fn orbit_startup_without_polaris_does_nothing() {
        let mut app = app();
        app.prepare_orbit_startup();
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn disabled_polaris_does_not_allocate_or_queue_work() {
        let mut app = app();
        let config = PolarisConfig {
            enabled: false,
            ..PolarisConfig::default()
        };
        app.prepare_polaris_startup(&config, true);
        assert!(app.state.workspaces.is_empty());
        assert!(app.pending_polaris_start.is_none());
    }
    #[test]
    fn restored_polaris_shell_is_reused_without_duplicate() {
        let mut app = app();
        let mut workspace = crate::workspace::Workspace::test_new("restored");
        workspace.custom_name = Some(LABEL.into());
        app.state.workspaces.push(workspace);
        app.prepare_polaris_startup(&PolarisConfig::default(), false);
        app.prepare_polaris_startup(&PolarisConfig::default(), false);
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.active, Some(0));
        assert!(app.pending_polaris_start.is_some());
    }
    #[test]
    fn restored_managed_polaris_is_not_launched_again() {
        let mut app = app();
        let mut workspace = crate::workspace::Workspace::test_new("restored");
        workspace.custom_name = Some(LABEL.into());
        let id = workspace
            .terminal_id(workspace.focused_pane_id().unwrap())
            .unwrap()
            .clone();
        let mut terminal = crate::terminal::TerminalState::new(id.clone(), "/".into());
        terminal.restore_managed_agent(NAME.into(), crate::detect::Agent::Claude);
        app.state.terminals.insert(id, terminal);
        app.state.workspaces.push(workspace);
        app.prepare_polaris_startup(&PolarisConfig::default(), false);
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(app.pending_polaris_start.is_none());
    }
    #[test]
    fn pending_start_waits_for_retry_time_without_touching_pane() {
        let mut app = app();
        let now = Instant::now();
        app.pending_polaris_start = Some(PendingPolarisStart {
            params: launch_params(&PolarisConfig::default(), "missing".into()).unwrap(),
            next_attempt: now + RETRY_INTERVAL,
            deadline: now + STARTUP_TIMEOUT,
        });
        assert!(!app.start_pending_polaris(now));
        assert!(app.pending_polaris_start.is_some());
        assert!(app.start_pending_polaris(now + STARTUP_TIMEOUT));
        assert!(app.pending_polaris_start.is_none());
        assert!(app.state.config_diagnostic.is_some());
        // Orbit activity exposes the same failure as lead_error.
        let activity = app.orbit_task_activity();
        assert!(activity["lead_error"]
            .as_str()
            .unwrap()
            .contains("Polaris could not start"));
    }
    #[test]
    fn lead_prompt_explains_worker_kind_and_model_choice() {
        for text in [
            "orbit_activity.kinds",
            "kind and model",
            "Codex",
            "allowlist",
        ] {
            assert!(LEAD_PROMPT.contains(text), "{text} missing");
        }
    }
    #[test]
    fn lead_prompt_covers_worker_lifecycle_isolation_and_layout() {
        for text in [
            "orbit_agent_stop",
            "Keep workers alive between tasks",
            "Reuse an idle worker",
            "worktree true",
            "stale_panes",
            "orbit_pane_resize",
            "orbit_pane_zoom",
            "orbit_pane_close",
        ] {
            assert!(LEAD_PROMPT.contains(text), "{text} missing");
        }
    }

    #[test]
    fn lead_prompt_routes_work_to_roles_as_the_ceo() {
        for text in [
            "CEO",
            "never do the work yourself",
            "responsible team role and only that role",
            "orbit_team_list",
            "orbit_agent_assign with role",
            "never exit",
            "orbit_team_remove",
            "Scratch workers in the Orbit tab are for one-off work only",
            "relay one task across them",
            "Do not reassign or cancel a task that is mid-relay",
        ] {
            assert!(LEAD_PROMPT.contains(text), "{text} missing");
        }
        assert!(!LEAD_PROMPT.chars().any(char::is_control));
    }

    #[test]
    fn lead_prompt_tells_polaris_to_check_untested_reports() {
        for text in [
            "tests_run",
            "checks",
            "changed_files",
            "run the tests yourself",
        ] {
            assert!(LEAD_PROMPT.contains(text), "{text} missing");
        }
    }

    #[test]
    fn lead_prompt_tells_polaris_to_leave_worker_approvals_to_the_user() {
        for text in [
            "blocked",
            "ask the user",
            "orbit_agent_approve",
            "only when",
            "permission_mode manual",
            "blocked worker with no assignment",
            "Decision line",
            "orbit_route",
            "always confirm",
        ] {
            assert!(LEAD_PROMPT.contains(text), "{text} missing");
        }
    }
    #[test]
    fn polaris_launch_is_interactive_claude_fable_without_permission_bypass() {
        let params = launch_params(&PolarisConfig::default(), "w1:p1".into()).unwrap();
        assert_eq!(params.name, "polaris");
        assert_eq!(params.kind, "claude");
        assert_eq!(&params.args[..2], &["--model", "fable"]);
        assert_eq!(params.args[2], "--append-system-prompt");
        assert!(!params
            .args
            .iter()
            .any(|argument| argument.contains("skip-permissions")));
    }
    #[test]
    fn invalid_provider_does_not_allocate_workspace() {
        let mut app = app();
        let config = PolarisConfig {
            provider: "unknown".into(),
            ..PolarisConfig::default()
        };
        app.prepare_polaris_startup(&config, true);
        assert!(app.state.workspaces.is_empty());
        assert!(app
            .state
            .config_diagnostic
            .as_deref()
            .unwrap()
            .contains("supports Claude"));
    }
}
