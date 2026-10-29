//! Session-owned Orbit binding and scoped operations. All resolution and mutation
//! happens on the server event loop; client focus and editable labels are ignored.
use serde::{Deserialize, Serialize};

use super::responses::encode_error;
use crate::api::schema::*;
use crate::app::App;
use crate::layout::PaneId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrbitBinding {
    workspace_id: String,
    tab_number: usize,
}

type OrbitError = (&'static str, String);

impl App {
    fn load_orbit_binding(&mut self) {
        if self.orbit_binding_loaded {
            return;
        }
        self.orbit_binding_loaded = true;
        if !self.policy.persist_session {
            return;
        }
        let path = crate::session::data_dir().join("orbit-binding.json");
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(binding) => self.orbit_binding = Some(binding),
                Err(error) => {
                    self.orbit_binding_error = Some(format!("Invalid Orbit binding: {error}"))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                self.orbit_binding_error = Some(format!("Cannot read Orbit binding: {error}"))
            }
        }
    }

    fn bind_orbit(&mut self, ws_idx: usize, tab_idx: usize) -> Result<(), String> {
        let binding = OrbitBinding {
            workspace_id: self.state.workspaces[ws_idx].id.clone(),
            tab_number: self.state.workspaces[ws_idx].tabs[tab_idx].number,
        };
        if self.policy.persist_session {
            let path = crate::session::data_dir().join("orbit-binding.json");
            let bytes = serde_json::to_vec(&binding).map_err(|e| e.to_string())?;
            // Private (0600) and atomic, like the other Orbit state files.
            crate::app::orbit_files::replace_atomically(&path, &bytes)
                .map_err(|e| e.to_string())?;
        }
        self.orbit_binding = Some(binding);
        self.orbit_binding_loaded = true;
        self.schedule_session_save();
        Ok(())
    }

    /// Existing labels are used only once to migrate the first Orbit prototype.
    /// A saved but missing binding must never silently acquire another tab.
    pub(crate) fn prepare_orbit_startup(&mut self) {
        self.provision_orbit_startup();
        if let Some(error) = &self.orbit_binding_error {
            tracing::warn!("{error}");
            self.state.config_diagnostic = Some(error.clone());
        }
    }

    fn provision_orbit_startup(&mut self) {
        self.load_orbit_binding();
        if self.orbit_binding.is_some() || self.orbit_binding_error.is_some() {
            return;
        }
        let polaris: Vec<_> = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.custom_name.as_deref() == Some("Polaris"))
            .map(|(index, _)| index)
            .collect();
        if polaris.is_empty() {
            return;
        }
        if polaris.len() != 1 {
            self.orbit_binding_error =
                Some("Multiple Polaris workspaces; cannot safely bind Orbit".into());
            return;
        }
        let ws_idx = polaris[0];
        let matches: Vec<_> = self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.custom_name.as_deref() == Some("Orbit"))
            .map(|(index, _)| index)
            .collect();
        if matches.len() > 1 {
            self.orbit_binding_error = Some("Multiple Orbit tabs; cannot safely choose one".into());
            return;
        }
        let tab_idx = if let Some(index) = matches.first() {
            *index
        } else {
            let response = self.handle_tab_create(
                "startup:orbit".into(),
                TabCreateParams {
                    workspace_id: Some(self.public_workspace_id(ws_idx)),
                    cwd: None,
                    focus: false,
                    label: Some("Orbit".into()),
                    env: Default::default(),
                },
            );
            let value: serde_json::Value = serde_json::from_str(&response).unwrap_or_default();
            if value.get("error").is_some() || value.get("result").is_none() {
                self.orbit_binding_error = Some(format!("Could not create Orbit: {response}"));
                return;
            }
            self.state.workspaces[ws_idx].tabs.len() - 1
        };
        if let Err(error) = self.bind_orbit(ws_idx, tab_idx) {
            self.orbit_binding_error = Some(format!("Could not save Orbit binding: {error}"));
        }
    }

    pub(crate) fn resolve_orbit(&mut self) -> Result<(usize, usize), OrbitError> {
        self.load_orbit_binding();
        if let Some(error) = &self.orbit_binding_error {
            return Err(("orbit_binding_invalid", error.clone()));
        }
        let binding = self.orbit_binding.as_ref().ok_or_else(|| {
            (
                "orbit_not_configured",
                "No Orbit binding exists in this session".into(),
            )
        })?;
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == binding.workspace_id)
            .ok_or_else(|| {
                (
                    "orbit_missing",
                    "The bound Orbit workspace no longer exists".into(),
                )
            })?;
        let tab_idx = self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .position(|tab| tab.number == binding.tab_number)
            .ok_or_else(|| {
                (
                    "orbit_missing",
                    "The bound Orbit tab no longer exists; no other tab was selected".into(),
                )
            })?;
        Ok((ws_idx, tab_idx))
    }

    pub(super) fn resolve_orbit_pane(
        &mut self,
        requested: Option<&str>,
    ) -> Result<(usize, PaneId, String), OrbitError> {
        let (ws_idx, tab_idx) = self.resolve_orbit()?;
        let tab = &self.state.workspaces[ws_idx].tabs[tab_idx];
        let pane_id = if let Some(requested) = requested {
            let (requested_ws, pane) =
                self.parse_current_public_pane_id(requested)
                    .ok_or_else(|| {
                        (
                            "orbit_scope_violation",
                            "Pane is not a current Orbit pane".into(),
                        )
                    })?;
            if requested_ws != ws_idx || !tab.panes.contains_key(&pane) {
                return Err((
                    "orbit_scope_violation",
                    "Pane is outside the bound Orbit tab".into(),
                ));
            }
            pane
        } else {
            tab.root_pane
        };
        if !tab.panes.contains_key(&pane_id) {
            return Err((
                "orbit_missing",
                "Orbit's root pane no longer exists; inspect and choose an Orbit pane".into(),
            ));
        }
        let public_id = self
            .public_pane_id(ws_idx, pane_id)
            .ok_or_else(|| ("orbit_missing", "Orbit pane identity is unavailable".into()))?;
        Ok((ws_idx, pane_id, public_id))
    }

    pub(super) fn handle_orbit_inspect(
        &mut self,
        id: String,
        params: OrbitInspectParams,
    ) -> String {
        if let Some(role) = params.role.as_deref() {
            return self.handle_orbit_role_inspect(id, role);
        }
        let (ws_idx, tab_idx) = match self.resolve_orbit() {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return encode_error(id, "orbit_missing", "Orbit tab is unavailable");
        };
        let panes: Vec<_> = self.state.workspaces[ws_idx].tabs[tab_idx]
            .layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane| self.pane_info(ws_idx, pane))
            .collect();
        let mut response = serde_json::to_value(SuccessResponse {
            id,
            result: ResponseResult::TabInfo { tab },
        })
        .unwrap();
        let mut panes = serde_json::to_value(panes).unwrap();
        self.annotate_orbit_panes(&mut panes);
        response["result"]["panes"] = panes;
        response.to_string()
    }

    pub(super) fn handle_orbit_read(&mut self, id: String, params: OrbitReadParams) -> String {
        let (_, _, pane_id) = match self.resolve_orbit_pane(params.pane_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        if params.lines.is_some_and(|lines| lines == 0 || lines > 2000) {
            return encode_error(id, "invalid_params", "lines must be between 1 and 2000");
        }
        self.handle_pane_read(
            id,
            PaneReadParams {
                pane_id,
                source: ReadSource::RecentUnwrapped,
                lines: Some(params.lines.unwrap_or(120)),
                format: ReadFormat::Text,
                strip_ansi: true,
                intent: Default::default(),
            },
        )
    }

    pub(super) fn handle_orbit_run(&mut self, id: String, params: OrbitRunParams) -> String {
        if params.command.trim().is_empty() || params.command.chars().any(char::is_control) {
            return encode_error(
                id,
                "invalid_params",
                "command must be a non-empty single-line command without control characters",
            );
        }
        let (ws_idx, pane, public_id) = match self.resolve_orbit_pane(params.pane_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane) else {
            return encode_error(id, "orbit_unavailable", "Orbit terminal is unavailable");
        };
        let terminal = self.state.terminals.get(terminal_id);
        if terminal.is_none_or(|terminal| {
            terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some()
        }) {
            return encode_error(
                id,
                "orbit_busy",
                "Orbit pane is occupied by an agent or is unavailable",
            );
        }
        let Some(runtime) = self.terminal_runtimes.get(terminal_id) else {
            return encode_error(id, "orbit_unavailable", "Orbit runtime is unavailable");
        };
        if crate::app::agents::available_shell_name(runtime).is_none() {
            return encode_error(
                id,
                "orbit_busy",
                "Orbit pane does not have an available foreground shell",
            );
        }
        self.handle_pane_send_input(
            id,
            PaneSendInputParams {
                pane_id: public_id,
                text: params.command,
                keys: vec!["Enter".into()],
            },
        )
    }

    pub(super) fn handle_orbit_split(&mut self, id: String, params: OrbitSplitParams) -> String {
        let (_, _, pane_id) = match self.resolve_orbit_pane(params.pane_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        self.handle_pane_split(
            id,
            PaneSplitParams {
                workspace_id: None,
                target_pane_id: Some(pane_id),
                direction: params.direction.unwrap_or(SplitDirection::Right),
                ratio: None,
                cwd: None,
                focus: false,
                right_click: Default::default(),
                env: Default::default(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    fn app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    fn bound_app() -> App {
        let mut app = app();
        app.state
            .workspaces
            .push(Workspace::test_new("orbit-workspace"));
        app.state
            .workspaces
            .push(Workspace::test_new("other-workspace"));
        app.state.workspaces[0].tabs[0].set_custom_name("Orbit".into());
        app.state.switch_workspace(1);
        app.bind_orbit(0, 0).unwrap();
        app
    }

    #[test]
    fn orbit_binding_survives_rename_and_workspace_reorder() {
        let mut app = bound_app();
        let expected = app.resolve_orbit_pane(None).unwrap().2;
        app.state.workspaces[0].custom_name = Some("renamed".into());
        app.state.workspaces[0].tabs[0].set_custom_name("renamed too".into());
        app.state.workspaces.swap(0, 1);
        assert_eq!(app.resolve_orbit_pane(None).unwrap().2, expected);
        let response: serde_json::Value =
            serde_json::from_str(&app.handle_orbit_inspect("inspect".into(), Default::default()))
                .unwrap();
        assert_eq!(response["result"]["tab"]["label"], "renamed too");
        assert!(response["result"]["panes"].is_array());
    }

    #[test]
    fn orbit_binding_survives_tab_reorder_and_rejects_sibling_tab() {
        let mut app = bound_app();
        let expected = app.resolve_orbit_pane(None).unwrap().2;
        let mut sibling = Workspace::test_new("sibling").tabs.remove(0);
        sibling.number = 2;
        let sibling_pane = sibling.root_pane;
        app.state.workspaces[0].tabs.insert(0, sibling);
        app.state.workspaces[0]
            .public_pane_numbers
            .insert(sibling_pane, 2);
        app.state.workspaces[0].next_public_pane_number = 3;
        app.state.workspaces[0].next_public_tab_number = 3;
        assert_eq!(app.resolve_orbit().unwrap(), (0, 1));
        assert_eq!(app.resolve_orbit_pane(None).unwrap().2, expected);
        let foreign = app.public_pane_id(0, sibling_pane).unwrap();
        assert_eq!(
            app.resolve_orbit_pane(Some(&foreign)).unwrap_err().0,
            "orbit_scope_violation"
        );
    }

    #[test]
    fn orbit_binding_rejects_foreign_panes_for_all_operations() {
        let mut app = bound_app();
        let foreign = app
            .public_pane_id(1, app.state.workspaces[1].tabs[0].root_pane)
            .unwrap();
        for response in [
            app.handle_orbit_run(
                "run".into(),
                OrbitRunParams {
                    pane_id: Some(foreign.clone()),
                    command: "echo test".into(),
                },
            ),
            app.handle_orbit_read(
                "read".into(),
                OrbitReadParams {
                    pane_id: Some(foreign.clone()),
                    lines: None,
                },
            ),
            app.handle_orbit_split(
                "split".into(),
                OrbitSplitParams {
                    pane_id: Some(foreign),
                    direction: None,
                },
            ),
        ] {
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], "orbit_scope_violation");
        }
    }

    #[test]
    fn orbit_binding_missing_target_never_adopts_same_label() {
        let mut app = bound_app();
        app.state.workspaces.remove(0);
        app.state.workspaces[0].custom_name = Some("Polaris".into());
        app.state.workspaces[0].tabs[0].set_custom_name("Orbit".into());
        app.prepare_orbit_startup();
        assert_eq!(app.resolve_orbit().unwrap_err().0, "orbit_missing");
    }

    #[test]
    fn orbit_binding_rejects_ambiguous_migration() {
        let mut app = app();
        let mut workspace = Workspace::test_new("polaris");
        workspace.custom_name = Some("Polaris".into());
        workspace.tabs[0].set_custom_name("Orbit".into());
        let mut extra = Workspace::test_new("extra").tabs.remove(0);
        extra.number = 2;
        extra.set_custom_name("Orbit".into());
        workspace.tabs.push(extra);
        app.state.workspaces.push(workspace);
        app.prepare_orbit_startup();
        assert!(app.orbit_binding.is_none());
        assert_eq!(app.resolve_orbit().unwrap_err().0, "orbit_binding_invalid");
    }

    #[test]
    fn orbit_run_rejects_managed_agent_pane() {
        let mut app = bound_app();
        let ws = &app.state.workspaces[0];
        let id = ws.terminal_id(ws.tabs[0].root_pane).unwrap().clone();
        let mut terminal = crate::terminal::TerminalState::new(id.clone(), "/".into());
        terminal.restore_managed_agent("busy".into(), crate::detect::Agent::Claude);
        app.state.terminals.insert(id, terminal);
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_run(
            "run".into(),
            OrbitRunParams {
                pane_id: None,
                command: "echo test".into(),
            },
        ))
        .unwrap();
        assert_eq!(response["error"]["code"], "orbit_busy");
    }

    #[tokio::test]
    async fn orbit_tools_split_and_read_real_work_tab_without_changing_focus() {
        let mut app = app();
        let mut workspace = Workspace::test_new("polaris");
        workspace.custom_name = Some("Polaris".into());
        app.state.workspaces.push(workspace);
        app.state.switch_workspace(0);
        let selected = app.state.workspaces[0].focused_pane_id();
        app.prepare_orbit_startup();
        let split: serde_json::Value = serde_json::from_str(
            &app.handle_orbit_split("split".into(), OrbitSplitParams::default()),
        )
        .unwrap();
        assert!(split.get("error").is_none(), "{split}");
        let read: serde_json::Value =
            serde_json::from_str(&app.handle_orbit_read("read".into(), OrbitReadParams::default()))
                .unwrap();
        assert!(read.get("error").is_none(), "{read}");
        let (ws, tab) = app.resolve_orbit().unwrap();
        assert_eq!(app.state.workspaces[ws].tabs[tab].panes.len(), 2);
        assert_eq!(app.state.workspaces[0].focused_pane_id(), selected);
    }

    #[tokio::test]
    async fn orbit_run_admits_command_only_to_bound_shell() {
        let mut app = bound_app();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        let terminal = crate::terminal::TerminalState::new(terminal_id.clone(), "/".into());
        app.state.terminals.insert(terminal_id.clone(), terminal);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let response: serde_json::Value = serde_json::from_str(&app.handle_orbit_run(
            "run".into(),
            OrbitRunParams {
                pane_id: None,
                command: "echo orbit".into(),
            },
        ))
        .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        let submitted = rx.try_recv().expect("command queued to Orbit runtime");
        assert!(String::from_utf8_lossy(&submitted).contains("echo orbit"));
        assert_eq!(app.state.active, Some(1));
    }

    #[test]
    fn orbit_binding_json_round_trip_keeps_stable_numbers() {
        let binding = OrbitBinding {
            workspace_id: "w7".into(),
            tab_number: 12,
        };
        let serialized = serde_json::to_string(&binding).unwrap();
        assert_eq!(
            serde_json::from_str::<OrbitBinding>(&serialized).unwrap(),
            binding
        );
    }
}
