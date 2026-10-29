//! Orbit worker records: what Polaris started (kind, model, worktree), panes left
//! behind by workers whose agent exited, and reuse of idle workers. Records are
//! runtime-only, keyed by the worker's stable terminal id, and scoped to the bound
//! Orbit tab: a pane that leaves Orbit loses its record.
use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};

use crate::api::schema::{AgentStatus, PaneRenameParams};
use crate::app::orbit_worker_policy::WorkerLaunch;
use crate::app::App;
use crate::layout::PaneId;

/// A worker's isolated checkout, created by `orbit.agent_start {worktree}` or
/// recovered after a restart from the worker's linked Git worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OrbitWorktree {
    pub(crate) path: PathBuf,
    /// `None` for a recovered checkout whose HEAD is detached or unreadable.
    pub(crate) branch: Option<String>,
    /// The checkout `git worktree remove` runs from.
    #[serde(skip)]
    pub(crate) repo_root: PathBuf,
    /// Recovered from the checkout after a restart rather than recorded at start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) recovered: bool,
    /// The branch was created for this worker, so discarding the worktree may delete it.
    #[serde(skip)]
    pub(crate) created_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerRecord {
    pub(crate) name: String,
    pub(crate) kind: String,
    /// The model the worker was launched with (requested or configured default).
    pub(crate) model: Option<String>,
    /// The Claude `--permission-mode` the worker was launched with.
    pub(crate) permission_mode: Option<&'static str>,
    pub(crate) worktree: Option<OrbitWorktree>,
    /// The agent exited and its pane label was marked `(exited)`.
    pub(crate) exited: bool,
}

pub(crate) type WorkerRecords = HashMap<String, WorkerRecord>;

/// Isolated worker starts between admission and launch.
#[derive(Debug, Default)]
pub(crate) struct OrbitStarts {
    /// Starts admitted and not yet finished, from request to launch.
    pub(crate) in_flight: usize,
    /// Terminal ids of new worker panes awaiting their launch; no other start
    /// may use them.
    pub(crate) reserved: std::collections::HashSet<String>,
}

const EXITED_SUFFIX: &str = " (exited)";

pub(super) fn exited_label(name: &str) -> String {
    format!("{name}{EXITED_SUFFIX}")
}

/// The worker name in an `<name> (exited)` label, which survives a restart in
/// the saved session while worker records do not.
fn exited_label_name(label: &str) -> Option<&str> {
    label
        .strip_suffix(EXITED_SUFFIX)
        .filter(|name| !name.is_empty())
}

impl WorkerRecord {
    pub(super) fn new(name: String, kind: String, model: Option<String>) -> Self {
        Self {
            name,
            kind,
            model,
            permission_mode: None,
            worktree: None,
            exited: false,
        }
    }

    /// A worker launched by Orbit with the admitted model and permission mode.
    pub(super) fn launched(name: String, kind: String, launch: WorkerLaunch) -> Self {
        Self {
            permission_mode: launch.permission_mode,
            ..Self::new(name, kind, launch.model)
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "kind": self.kind,
            "model": self.model,
            "permission_mode": self.permission_mode,
            "worktree": self.worktree,
        })
    }
}

impl App {
    /// Terminal id to pane for every pane in the bound Orbit tab, in layout order.
    pub(crate) fn orbit_tab_terminals(&mut self) -> Vec<(String, usize, PaneId)> {
        let Ok((ws, tab)) = self.resolve_orbit() else {
            return Vec::new();
        };
        let workspace = &self.state.workspaces[ws];
        workspace.tabs[tab]
            .layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane| Some((workspace.terminal_id(pane)?.to_string(), ws, pane)))
            .collect()
    }

    pub(super) fn orbit_terminal_hosts_agent(&self, terminal_id: &str) -> bool {
        self.state
            .terminals
            .get(terminal_id)
            .is_some_and(|terminal| terminal.is_agent_terminal())
    }

    /// Called for every pane state change; only recorded Orbit workers do work.
    pub(crate) fn note_orbit_worker_update(&mut self, ws_idx: usize, pane_id: PaneId) {
        if self.orbit_workers.is_empty() {
            return;
        }
        let recorded = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .is_some_and(|terminal_id| self.orbit_workers.contains_key(terminal_id.as_str()));
        if recorded {
            self.refresh_orbit_worker_records();
        }
    }

    /// Prunes records that left Orbit, and relabels panes whose worker agent
    /// exited as `<name> (exited)` once, so the lead can find and close them.
    pub(crate) fn refresh_orbit_worker_records(&mut self) {
        if self.orbit_workers.is_empty() {
            return;
        }
        let panes: HashMap<String, (usize, PaneId)> = self
            .orbit_tab_terminals()
            .into_iter()
            .map(|(terminal_id, ws, pane)| (terminal_id, (ws, pane)))
            .collect();
        self.orbit_workers
            .retain(|terminal_id, _| panes.contains_key(terminal_id));
        let mut exited = Vec::new();
        let mut replaced = Vec::new();
        for (terminal_id, record) in &self.orbit_workers {
            let live = self.orbit_terminal_hosts_agent(terminal_id);
            match (record.exited, live) {
                (false, false) => exited.push(terminal_id.clone()),
                // Another agent now runs in a stale pane; it is not this worker.
                (true, true) => replaced.push(terminal_id.clone()),
                _ => {}
            }
        }
        for terminal_id in replaced {
            let Some(record) = self.orbit_workers.remove(&terminal_id) else {
                continue;
            };
            let stale_label =
                self.state
                    .terminals
                    .get(terminal_id.as_str())
                    .is_some_and(|terminal| {
                        terminal.manual_label.as_deref()
                            == Some(exited_label(&record.name).as_str())
                    });
            if stale_label {
                let (ws, pane) = panes[&terminal_id];
                self.set_orbit_pane_label(ws, pane, None);
            }
        }
        for terminal_id in exited {
            let Some(record) = self.orbit_workers.get_mut(&terminal_id) else {
                continue;
            };
            record.exited = true;
            let label = exited_label(&record.name);
            tracing::info!(%terminal_id, %label, "orbit worker agent exited");
            let (ws, pane) = panes[&terminal_id];
            self.set_orbit_pane_label(ws, pane, Some(label));
        }
    }

    /// Renames an Orbit pane and publishes the change to clients.
    pub(super) fn set_orbit_pane_label(&mut self, ws: usize, pane: PaneId, label: Option<String>) {
        let Some(pane_id) = self.public_pane_id(ws, pane) else {
            return;
        };
        self.handle_pane_rename("orbit:label".into(), PaneRenameParams { pane_id, label });
        self.schedule_session_save();
        self.emit_pane_updated(ws, pane);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// The name of the exited worker whose pane this is: a recorded worker whose
    /// agent left, or after a restart an unrecorded pane still labelled
    /// `<name> (exited)` with no agent.
    fn orbit_stale_worker_name(&self, terminal_id: &str) -> Option<String> {
        if self.orbit_terminal_hosts_agent(terminal_id) {
            return None;
        }
        match self.orbit_workers.get(terminal_id) {
            Some(record) => record.exited.then(|| record.name.clone()),
            None => self
                .state
                .terminals
                .get(terminal_id)?
                .manual_label
                .as_deref()
                .and_then(exited_label_name)
                .map(str::to_owned),
        }
    }

    pub(super) fn orbit_terminal_is_stale(&self, terminal_id: &str) -> bool {
        self.orbit_stale_worker_name(terminal_id).is_some()
    }

    pub(super) fn orbit_worker_json(&self, terminal_id: &str) -> Option<Value> {
        self.orbit_workers
            .get(terminal_id)
            .map(WorkerRecord::to_json)
    }

    /// Panes in Orbit whose recorded worker exited; close them with orbit_pane_close.
    pub(super) fn orbit_stale_panes(&mut self) -> Vec<Value> {
        self.refresh_orbit_worker_records();
        self.orbit_tab_terminals()
            .into_iter()
            .filter_map(|(terminal_id, ws, pane)| {
                let name = self.orbit_stale_worker_name(&terminal_id)?;
                Some(json!({
                    "pane_id": self.public_pane_id(ws, pane)?,
                    "terminal_id": terminal_id,
                    "label": exited_label(&name),
                    "name": name,
                }))
            })
            .collect()
    }

    /// An idle, unassigned, non-isolated Orbit worker of `kind`, launched with
    /// `model` and `permission_mode` when either is requested. Workers without a
    /// record (restored after a restart) match only when neither is requested.
    pub(super) fn find_reusable_orbit_worker(
        &mut self,
        kind: &str,
        model: Option<&str>,
        permission_mode: Option<&str>,
    ) -> Option<(usize, PaneId, String)> {
        let kind = crate::detect::parse_agent_label(kind)?;
        self.refresh_orbit_worker_records();
        // A worker restored after a restart may sit in a worktree; never reuse it
        // for work outside its checkout.
        self.recover_orbit_worktree_workers();
        for (terminal_id, ws, pane) in self.orbit_tab_terminals() {
            if self.orbit_is_lead_terminal(&terminal_id)
                || self.orbit_terminal_is_stale(&terminal_id)
            {
                continue;
            }
            let Some(agent) = self.agent_info(ws, pane) else {
                continue;
            };
            let managed_kind = self
                .state
                .terminals
                .get(terminal_id.as_str())
                .and_then(|terminal| terminal.managed_agent_kind());
            let record = self.orbit_workers.get(&terminal_id);
            let model_matches = match (model, record) {
                (None, _) => true,
                (Some(model), Some(record)) => record.model.as_deref() == Some(model),
                (Some(_), None) => false,
            };
            let mode_matches = match (permission_mode, record) {
                (None, _) => true,
                (Some(mode), Some(record)) => record.permission_mode == Some(mode),
                (Some(_), None) => false,
            };
            if managed_kind != Some(kind)
                || !model_matches
                || !mode_matches
                || record.is_some_and(|record| record.worktree.is_some())
                || !matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done)
                || !agent.interactive_ready
                || self.orbit_worker_has_open_task(&terminal_id)
            {
                continue;
            }
            return Some((ws, pane, terminal_id));
        }
        None
    }

    /// Adds `stale` and the recorded `worker` (kind, model, worktree) to each
    /// pane of `orbit.inspect`.
    pub(super) fn annotate_orbit_panes(&mut self, panes: &mut Value) {
        self.refresh_orbit_worker_records();
        self.recover_orbit_worktree_workers();
        let Some(panes) = panes.as_array_mut() else {
            return;
        };
        for pane in panes {
            let Some(terminal_id) = pane["terminal_id"].as_str().map(str::to_owned) else {
                continue;
            };
            pane["stale"] = json!(self.orbit_terminal_is_stale(&terminal_id));
            if let Some(worker) = self.orbit_worker_json(&terminal_id) {
                pane["worker"] = worker;
            }
        }
    }

    /// The recorded model and worktree, merged into an `agent.get`-shaped response.
    pub(super) fn annotate_orbit_agent_response(&mut self, response: String) -> String {
        let Ok(mut value) = serde_json::from_str::<Value>(&response) else {
            return response;
        };
        let Some(terminal_id) = value["result"]["agent"]["terminal_id"]
            .as_str()
            .map(str::to_owned)
        else {
            return response;
        };
        self.recover_orbit_worktree_workers();
        let record = self.orbit_workers.get(&terminal_id);
        value["result"]["worker"] = json!({
            "model": record.and_then(|record| record.model.clone()),
            "permission_mode": record.and_then(|record| record.permission_mode),
            "worktree": record.and_then(|record| record.worktree.clone()),
        });
        value.to_string()
    }

    /// The instruction line naming an isolated worker's branch and checkout.
    pub(crate) fn orbit_worktree_instruction(&self, worker_id: &str) -> Option<String> {
        let worktree = self.orbit_workers.get(worker_id)?.worktree.as_ref()?;
        let branch = worktree
            .branch
            .as_deref()
            .map(|branch| format!(" on branch {branch}"))
            .unwrap_or_default();
        Some(format!(
            "\n\nYou work in an isolated Git worktree{branch} at {}; make and commit your changes there only.",
            worktree.path.display()
        ))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    /// Polaris workspace: tab 0 holds the bound lead, the Orbit tab (tab 1) holds
    /// a root pane plus one split; the user focuses another workspace.
    pub(in crate::app::api) fn orbit_app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut polaris = Workspace::test_new("polaris");
        polaris.custom_name = Some("Polaris".into());
        polaris.tabs[0].set_custom_name("Polaris".into());
        let orbit_tab = polaris.test_add_tab(Some("Orbit"));
        polaris.active_tab = orbit_tab;
        polaris.test_split(ratatui::layout::Direction::Horizontal);
        polaris.active_tab = 0;
        app.state.workspaces.push(polaris);
        app.state.workspaces.push(Workspace::test_new("elsewhere"));
        app.prepare_orbit_startup();
        assert_eq!(app.resolve_orbit().unwrap(), (0, 1));
        app.state.ensure_test_terminals();
        app.state.switch_workspace(1);
        let lead = terminal_of(&app, 0, app.state.workspaces[0].tabs[0].root_pane);
        let terminal = app.state.terminals.get_mut(lead.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        terminal.set_agent_name(crate::app::polaris::NAME.into());
        app.bind_orbit_lead(lead);
        app
    }

    pub(in crate::app::api) fn terminal_of(app: &App, ws: usize, pane: PaneId) -> String {
        app.state.workspaces[ws]
            .terminal_id(pane)
            .unwrap()
            .to_string()
    }

    /// Orbit panes in layout order as (pane, public id, terminal id).
    pub(in crate::app::api) fn orbit_panes(app: &mut App) -> Vec<(PaneId, String, String)> {
        app.orbit_tab_terminals()
            .into_iter()
            .map(|(terminal, ws, pane)| (pane, app.public_pane_id(ws, pane).unwrap(), terminal))
            .collect()
    }

    /// Makes the Orbit pane at `index` a ready, idle managed worker.
    pub(in crate::app::api) fn add_worker(
        app: &mut App,
        index: usize,
        name: &str,
        model: Option<&str>,
    ) -> String {
        let (_, _, terminal_id) = orbit_panes(app)[index].clone();
        let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
        terminal.restore_managed_agent(name.into(), Agent::Claude);
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        terminal.set_manual_label(name.into());
        app.orbit_workers.insert(
            terminal_id.clone(),
            WorkerRecord::new(name.into(), "claude".into(), model.map(Into::into)),
        );
        terminal_id
    }

    /// The agent process left: detection clears the agent and its name.
    pub(in crate::app::api) fn exit_worker(app: &mut App, terminal_id: &str) {
        let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
        terminal.clear_agent_name();
        terminal.set_detected_state(None, AgentState::Unknown);
    }

    fn label(app: &App, terminal_id: &str) -> Option<String> {
        app.state.terminals[terminal_id].manual_label.clone()
    }

    #[test]
    fn exited_worker_is_relabelled_stale_and_listed() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        assert!(app.orbit_stale_panes().is_empty());
        exit_worker(&mut app, &worker);
        let (ws, _) = app.resolve_orbit().unwrap();
        let pane = orbit_panes(&mut app)[1].0;
        app.note_orbit_worker_update(ws, pane);
        assert_eq!(label(&app, &worker).as_deref(), Some("reviewer (exited)"));
        let stale = app.orbit_stale_panes();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0]["terminal_id"], worker);
        assert_eq!(stale[0]["name"], "reviewer");
        let inspect: Value =
            serde_json::from_str(&app.handle_orbit_inspect("inspect".into(), Default::default()))
                .unwrap();
        let panes = inspect["result"]["panes"].as_array().unwrap();
        let entry = panes
            .iter()
            .find(|pane| pane["terminal_id"] == worker)
            .unwrap();
        assert_eq!(entry["stale"], true);
        assert_eq!(entry["worker"]["name"], "reviewer");
        assert!(panes.iter().any(|pane| pane["stale"] == false));
        assert_eq!(app.state.active, Some(1));
    }

    #[test]
    fn stale_detection_ignores_unrecorded_panes_and_panes_outside_orbit() {
        let mut app = orbit_app();
        let (_, _, unrecorded) = orbit_panes(&mut app)[1].clone();
        let terminal = app.state.terminals.get_mut(unrecorded.as_str()).unwrap();
        terminal.set_manual_label("manual".into());
        let outside = terminal_of(&app, 1, app.state.workspaces[1].tabs[0].root_pane);
        app.orbit_workers.insert(
            outside.clone(),
            WorkerRecord::new("outside".into(), "claude".into(), None),
        );
        app.refresh_orbit_worker_records();
        assert!(app.orbit_stale_panes().is_empty());
        assert_eq!(label(&app, &unrecorded).as_deref(), Some("manual"));
        assert!(!app.orbit_workers.contains_key(&outside));
    }

    #[test]
    fn new_agent_in_stale_pane_drops_the_old_record() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        exit_worker(&mut app, &worker);
        app.refresh_orbit_worker_records();
        assert!(app.orbit_terminal_is_stale(&worker));
        let terminal = app.state.terminals.get_mut(worker.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        app.refresh_orbit_worker_records();
        assert!(!app.orbit_workers.contains_key(&worker));
    }

    #[test]
    fn another_agent_in_a_stale_pane_clears_the_exited_label() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        exit_worker(&mut app, &worker);
        app.refresh_orbit_worker_records();
        assert_eq!(label(&app, &worker).as_deref(), Some("reviewer (exited)"));
        let terminal = app.state.terminals.get_mut(worker.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        app.refresh_orbit_worker_records();
        assert_eq!(label(&app, &worker), None);
        assert!(app.orbit_stale_panes().is_empty());
    }

    #[test]
    fn restored_exited_label_without_a_record_is_a_stale_pane() {
        let mut app = orbit_app();
        let (_, pane_id, terminal_id) = orbit_panes(&mut app)[1].clone();
        let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
        terminal.set_manual_label("reviewer (exited)".into());
        let stale = app.orbit_stale_panes();
        assert_eq!(stale.len(), 1, "{stale:?}");
        assert_eq!(stale[0]["pane_id"], json!(pane_id));
        assert_eq!(stale[0]["name"], "reviewer");
        assert_eq!(stale[0]["label"], "reviewer (exited)");
        // A live agent under that label is not stale, nor is a bare suffix.
        let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        assert!(app.orbit_stale_panes().is_empty());
        assert_eq!(exited_label_name(" (exited)"), None);
    }

    #[test]
    fn reuse_matches_kind_and_requested_model_only() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", Some("opus"));
        assert_eq!(
            app.find_reusable_orbit_worker("claude", None, None)
                .unwrap()
                .2,
            worker
        );
        assert_eq!(
            app.find_reusable_orbit_worker("claude", Some("opus"), None)
                .unwrap()
                .2,
            worker
        );
        assert!(app
            .find_reusable_orbit_worker("claude", Some("sonnet"), None)
            .is_none());
        assert!(app
            .find_reusable_orbit_worker("codex", None, None)
            .is_none());
        // The lead is never reused, even as an idle Claude agent.
        app.orbit_workers.clear();
        assert!(app
            .find_reusable_orbit_worker("claude", Some("opus"), None)
            .is_none());
    }

    #[test]
    fn reuse_skips_busy_stale_and_isolated_workers() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let terminal = app.state.terminals.get_mut(worker.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        assert!(app
            .find_reusable_orbit_worker("claude", None, None)
            .is_none());
        let terminal = app.state.terminals.get_mut(worker.as_str()).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        app.orbit_workers.get_mut(&worker).unwrap().worktree = Some(OrbitWorktree {
            path: "/tmp/wt".into(),
            branch: Some("b".into()),
            repo_root: "/tmp/repo".into(),
            recovered: false,
            created_branch: false,
        });
        assert!(app
            .find_reusable_orbit_worker("claude", None, None)
            .is_none());
        app.orbit_workers.get_mut(&worker).unwrap().worktree = None;
        exit_worker(&mut app, &worker);
        assert!(app
            .find_reusable_orbit_worker("claude", None, None)
            .is_none());
    }

    #[test]
    fn worktree_instruction_names_branch_and_path() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "editor", None);
        assert!(app.orbit_worktree_instruction(&worker).is_none());
        app.orbit_workers.get_mut(&worker).unwrap().worktree = Some(OrbitWorktree {
            path: "/tmp/wt/editor".into(),
            branch: Some("worktree/calm-river-0001".into()),
            repo_root: "/tmp/repo".into(),
            recovered: false,
            created_branch: false,
        });
        let line = app.orbit_worktree_instruction(&worker).unwrap();
        assert!(line.contains("worktree/calm-river-0001"), "{line}");
        assert!(line.contains("/tmp/wt/editor"), "{line}");
        assert_eq!(line.trim().lines().count(), 1);
    }
}
