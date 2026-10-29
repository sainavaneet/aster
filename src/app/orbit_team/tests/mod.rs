//! Team roster, supervisor, and routing tests. Role tabs built here use test
//! runtimes, so a launch only queues bytes and never starts a real agent.
use serde_json::{json, Value};
use tokio::sync::mpsc::Receiver;

use crate::app::App;
use crate::detect::{Agent, AgentState};
use crate::workspace::Workspace;

use super::roster::{self, RoleSpec};

mod pinned;
mod roster_tests;
mod routing;
mod shell_view;
mod startup;
mod supervision;

pub(super) type Queue = Receiver<bytes::Bytes>;

pub(super) fn new_app() -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        rx,
        crate::api::EventHub::default(),
    )
}

/// Makes `ws` the Polaris workspace: tab 0 holds the bound lead, a new `Orbit`
/// tab is bound as the scratch tab, and the user focuses `focus`.
pub(super) fn bind_polaris(app: &mut App, ws: usize, focus: usize) {
    let workspace = &mut app.state.workspaces[ws];
    workspace.custom_name = Some("Polaris".into());
    workspace.tabs[0].set_custom_name("Polaris".into());
    workspace.test_add_tab(Some("Orbit"));
    app.prepare_orbit_startup();
    app.state.ensure_test_terminals();
    let lead = terminal(app, ws, 0, app.state.workspaces[ws].tabs[0].root_pane);
    let state = app.state.terminals.get_mut(lead.as_str()).unwrap();
    state.set_detected_state(Some(Agent::Claude), AgentState::Idle);
    state.set_agent_name(crate::app::polaris::NAME.into());
    app.bind_orbit_lead(lead);
    app.state.switch_workspace(focus);
}

/// Polaris workspace 0 plus a focused workspace 1.
pub(super) fn team_app() -> App {
    let mut app = new_app();
    app.state.workspaces.push(Workspace::test_new("polaris"));
    app.state.workspaces.push(Workspace::test_new("elsewhere"));
    bind_polaris(&mut app, 0, 1);
    app
}

pub(super) fn terminal(app: &App, ws: usize, _tab: usize, pane: crate::layout::PaneId) -> String {
    app.state.workspaces[ws]
        .terminal_id(pane)
        .unwrap()
        .to_string()
}

pub(super) fn spec(name: &str, members: &[(&str, &str)]) -> RoleSpec {
    RoleSpec {
        name: name.into(),
        area: format!("{name} area"),
        prompt: format!("Act as the {name}."),
        members: members
            .iter()
            .map(|(member, kind)| roster::MemberSpec {
                name: (*member).into(),
                kind: (*kind).into(),
                model: Some((*member).into()),
                permission_mode: None,
            })
            .collect(),
    }
}

/// Adds a role whose tab (one pane per member, each a test shell) sits before
/// the Orbit tab, as `orbit_team_define` places it. Returns the pane queues.
pub(super) fn add_role(app: &mut App, spec: RoleSpec) -> Vec<Queue> {
    let members = spec.members.len();
    let mut role = roster::validate(spec, &app.orbit_worker_policy, None).unwrap();
    let (ws, orbit_tab) = app.resolve_orbit().unwrap();
    let workspace = &mut app.state.workspaces[ws];
    let created = workspace.test_add_tab(Some(&role.label()));
    let previous = workspace.active_tab;
    workspace.active_tab = created;
    for _ in 1..members {
        workspace.test_split(ratatui::layout::Direction::Horizontal);
    }
    workspace.active_tab = previous;
    assert!(workspace.move_tab(created, orbit_tab));
    let tab = orbit_tab;
    role.tab = Some(roster::TabBinding {
        workspace_id: workspace.id.clone(),
        tab_number: workspace.tabs[tab].number,
    });
    app.orbit_team.roster.upsert(role).unwrap();
    app.state.ensure_test_terminals();
    app.activate_orbit_team_supervisor();
    let panes = app.state.workspaces[ws].tabs[tab].layout.pane_ids();
    panes
        .into_iter()
        .map(|pane| {
            let id = app.state.workspaces[ws].terminal_id(pane).unwrap().clone();
            let (runtime, queue) =
                crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 16);
            app.terminal_runtimes.insert(id, runtime);
            queue
        })
        .collect()
}

/// Everything queued to a pane so far, as text.
pub(super) fn drain(queue: &mut Queue) -> String {
    let mut text = String::new();
    while let Ok(bytes) = queue.try_recv() {
        text.push_str(&String::from_utf8_lossy(&bytes));
    }
    text
}

/// Text queued to a pane until it contains `needle`; prompts are written by a
/// background submission, so they arrive shortly after the request returns.
pub(super) async fn wait_text(queue: &mut Queue, needle: &str) -> String {
    let mut text = String::new();
    for _ in 0..400 {
        text.push_str(&drain(queue));
        if text.contains(needle) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    text
}

/// The terminal hosting `member`, made a ready, idle managed agent.
pub(super) fn ready_member(app: &mut App, role: &str, member: &str) -> String {
    let role = app.orbit_team.roster.role(role).unwrap().clone();
    let (ws, tab) = app.resolve_role_tab(&role).unwrap();
    let (_, terminal_id) = app
        .orbit_member_pane(ws, tab, member)
        .expect("member launched");
    let kind = app
        .orbit_team
        .roster
        .member(member)
        .unwrap()
        .1
        .agent()
        .unwrap();
    let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
    terminal.restore_managed_agent(member.into(), kind);
    terminal.set_detected_state(Some(kind), AgentState::Idle);
    terminal_id
}

/// The member's agent process left the pane.
pub(super) fn exit_member(app: &mut App, terminal_id: &str) {
    let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
    terminal.clear_agent_name();
    terminal.reconcile_managed_agent_at(std::time::Instant::now(), true);
    terminal.set_detected_state(None, AgentState::Unknown);
}

pub(super) fn call(app: &mut App, method: &str, params: Value) -> Value {
    let request: crate::api::schema::Request =
        serde_json::from_value(json!({"id": "test", "method": method, "params": params})).unwrap();
    serde_json::from_str(&app.handle_api_request(request)).unwrap()
}

pub(super) fn focus(app: &App) -> (Option<usize>, Vec<usize>) {
    (
        app.state.active,
        app.state
            .workspaces
            .iter()
            .map(|ws| ws.active_tab)
            .collect(),
    )
}

pub(super) fn tab_labels(app: &App, ws: usize) -> Vec<String> {
    app.state.workspaces[ws]
        .tabs
        .iter()
        .map(|tab| tab.custom_name.clone().unwrap_or_default())
        .collect()
}
