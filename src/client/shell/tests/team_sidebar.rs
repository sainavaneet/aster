use super::*;
use crate::protocol::{OrbitShellLead, OrbitShellMember, OrbitShellRole, OrbitShellView};

pub(super) fn member(
    name: &str,
    kind: &str,
    pane_id: Option<&str>,
    state: &str,
) -> OrbitShellMember {
    OrbitShellMember {
        name: name.into(),
        kind: kind.into(),
        terminal_id: pane_id.map(|pane_id| format!("term_{pane_id}")),
        pane_id: pane_id.map(str::to_owned),
        state: state.into(),
        model: None,
    }
}

pub(super) fn orbit() -> OrbitShellView {
    OrbitShellView {
        lead: OrbitShellLead {
            name: "Polaris".into(),
            terminal_id: "term_lead".into(),
            pane_id: Some("pane_1".into()),
            state: "idle".into(),
        },
        roles: vec![
            OrbitShellRole {
                name: "reviewer".into(),
                pinned: true,
                members: vec![
                    member("rev-codex", "codex", Some("pane_3"), "blocked"),
                    member("rev-claude", "claude", Some("pane_2"), "working"),
                    member("rev-gone", "claude", None, "gave_up"),
                ],
            },
            OrbitShellRole {
                name: "ui".into(),
                pinned: false,
                members: Vec::new(),
            },
        ],
        scratch: vec![member("probe", "claude", Some("pane_4"), "idle")],
        active_tasks: Vec::new(),
        recent_tasks: Vec::new(),
        server_now_ms: None,
    }
}

pub(super) fn agent(pane_id: &str, name: &str, status: AgentStatus, seq: u64) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some(name.into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: pane_id == "pane_1",
    }
}

pub(super) fn team_snapshot() -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.agents = vec![
        agent("pane_1", "polaris-agent", AgentStatus::Idle, 1),
        agent("pane_2", "claude-agent", AgentStatus::Working, 2),
        agent("pane_3", "codex-agent", AgentStatus::Blocked, 3),
        agent("pane_4", "probe-agent", AgentStatus::Idle, 4),
        agent("pane_8", "stray-idle", AgentStatus::Idle, 5),
        agent("pane_9", "stray-blocked", AgentStatus::Blocked, 6),
    ];
    projected.orbit = Some(orbit());
    projected
}

pub(super) fn team_state(config: Config, projected: ClientShellSnapshot) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

/// Sidebar text rows (everything left of the divider) for a composed frame.
pub(super) fn sidebar_rows(frame: &FrameData, state: &ClientShellState) -> Vec<String> {
    let width = state.hits.agent_body.right() as usize;
    frame_rows(frame)
        .into_iter()
        .map(|row| row.chars().take(width).collect())
        .collect()
}

pub(super) fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("sidebar has no row containing {needle:?}: {rows:#?}"))
}

/// Every pane click target in the frame: team rows (resolved through the
/// cached rows), then flat-list or collapsed-sidebar rows.
pub(super) fn pane_hits(state: &ClientShellState) -> Vec<(Rect, &str)> {
    let team = state.hits.team_agents.iter().map(|(rect, line)| {
        let pane_id = state
            .team_sidebar
            .as_ref()
            .and_then(|team| team.line_pane_id(*line))
            .expect("a team hit resolves to a pane");
        (*rect, pane_id)
    });
    team.chain(
        state
            .hits
            .agents
            .iter()
            .map(|(rect, pane_id)| (*rect, pane_id.as_str())),
    )
    .collect()
}

pub(super) fn hit_panes(state: &ClientShellState) -> Vec<&str> {
    pane_hits(state)
        .into_iter()
        .map(|(_, pane_id)| pane_id)
        .collect()
}

pub(super) fn click_event(rect: Rect) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })
}

pub(super) fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![click_event(rect)])
}

pub(super) fn focused_pane(input: &ClientShellInput) -> Option<&str> {
    match &input.actions[..] {
        [ClientShellAction::Endpoint { request, .. }] => match &request.method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.as_str()),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn team_tree_lists_lead_roles_scratch_then_other_agents() {
    let mut config = Config::default();
    config.ui.style = crate::config::UiStyleConfig::Classic;
    config.ui.sidebar_width = 44;
    config.ui.sidebar_max_width = 60;
    let mut state = team_state(config, team_snapshot());
    let frame = state.compose(106, 40).expect("team sidebar frame");
    let rows = sidebar_rows(&frame, &state);

    let order = [
        "team",
        "Polaris",
        "reviewer",
        "rev-codex",
        "rev-claude",
        "rev-gone",
        "ui",
        "scratch",
        "probe",
        "other",
        "stray-idle",
        "stray-blocked",
    ]
    .map(|needle| row_of(&rows, needle));
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "rows out of order: {order:?}\n{rows:#?}"
    );
    assert!(rows[row_of(&rows, "Polaris")].contains("lead"));
    assert!(rows[row_of(&rows, "reviewer")].contains("▾ reviewer 3 pinned"));
    assert!(rows[row_of(&rows, "rev-codex")].contains("codex"));
    assert!(
        rows[row_of(&rows, "rev-gone")].contains("claude · gave up"),
        "a member without a live agent shows its supervision phase"
    );
    // The flat list's own header must not appear above the tree.
    assert!(!rows.iter().any(|row| row.contains("agents")));
    // Role members keep roster order, then the non-team agents in "spaces" order.
    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_3", "pane_2", "pane_4", "pane_8", "pane_9"]
    );
    assert_eq!(state.hits.team_groups.len(), 4);
}

#[test]
fn agent_panel_sort_orders_other_agents_but_never_role_members() {
    let mut config = Config::default();
    config.ui.agent_panel_sort = crate::config::AgentPanelSortConfig::Priority;
    let mut state = team_state(config, team_snapshot());
    state.compose(106, 40).expect("priority team frame");

    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_3", "pane_2", "pane_4", "pane_9", "pane_8"],
        "roster order inside a role; blocked stray first among the rest"
    );
}

#[test]
fn clicking_a_member_focuses_its_pane_and_a_paneless_member_is_inert() {
    let mut state = team_state(Config::default(), team_snapshot());
    let frame = state.compose(106, 40).expect("team sidebar frame");
    let rows = sidebar_rows(&frame, &state);

    let claude_row = row_of(&rows, "rev-claude") as u16;
    let rect = pane_hits(&state)
        .into_iter()
        .find(|(rect, pane_id)| rect.y == claude_row && *pane_id == "pane_2")
        .expect("rev-claude row hit")
        .0;
    let input = click(&mut state, rect);
    assert_eq!(focused_pane(&input), Some("pane_2"));

    let gone_row = row_of(&rows, "rev-gone") as u16;
    assert!(
        !pane_hits(&state).iter().any(|(rect, _)| rect.y == gone_row),
        "a member with no pane has no focus hit"
    );
    let input = click(&mut state, Rect::new(rect.x + 2, gone_row, 1, 1));
    assert!(input.actions.is_empty());
}

#[test]
fn clicking_a_group_header_collapses_it_without_a_request() {
    let mut config = Config::default();
    config.ui.style = crate::config::UiStyleConfig::Classic;
    let mut state = team_state(config, team_snapshot());
    let frame = state.compose(106, 40).expect("team sidebar frame");
    let header = row_of(&sidebar_rows(&frame, &state), "reviewer") as u16;
    let rect = state
        .hits
        .team_groups
        .iter()
        .find(|(rect, _)| rect.y == header)
        .expect("reviewer header hit")
        .0;

    let input = click(&mut state, rect);
    assert!(input.actions.is_empty(), "collapse is client-local");
    let frame = state.compose(106, 40).expect("collapsed group frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(rows[row_of(&rows, "reviewer")].contains("▸ reviewer"));
    assert!(!rows.iter().any(|row| row.contains("rev-claude")));
    assert_eq!(hit_panes(&state), ["pane_1", "pane_4", "pane_8", "pane_9"]);

    click(&mut state, rect);
    let frame = state.compose(106, 40).expect("expanded group frame");
    assert!(sidebar_rows(&frame, &state)
        .iter()
        .any(|row| row.contains("rev-claude")));
}

#[test]
fn collapsing_the_other_group_hides_non_team_agents() {
    let mut state = team_state(Config::default(), team_snapshot());
    let frame = state.compose(106, 40).expect("team sidebar frame");
    let header = row_of(&sidebar_rows(&frame, &state), "other") as u16;
    let rect = state
        .hits
        .team_groups
        .iter()
        .find(|(rect, _)| rect.y == header)
        .expect("other header hit")
        .0;
    click(&mut state, rect);
    state.compose(106, 40).expect("collapsed other frame");
    assert_eq!(hit_panes(&state), ["pane_1", "pane_3", "pane_2", "pane_4"]);
}

#[test]
fn sidebar_team_false_restores_the_flat_agent_list() {
    let mut config = Config::default();
    config.ui.sidebar_team = false;
    let mut state = team_state(config, team_snapshot());
    let frame = state.compose(106, 40).expect("flat sidebar frame");
    let rows = sidebar_rows(&frame, &state);

    assert!(rows.iter().any(|row| row.contains("agents")));
    assert!(!rows.iter().any(|row| row.contains("Polaris")));
    assert!(state.hits.team_groups.is_empty());
    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_2", "pane_3", "pane_4", "pane_8", "pane_9"]
    );
}

#[test]
fn a_session_without_a_team_keeps_the_flat_list() {
    let mut projected = team_snapshot();
    projected.orbit = None;
    let mut state = team_state(Config::default(), projected);
    state.compose(106, 40).expect("no team frame");
    assert!(state.team_sidebar.is_none());
    assert!(state.hits.team_groups.is_empty());
    assert_eq!(hit_panes(&state).len(), 6);
}

#[test]
fn team_rows_follow_the_orbit_projection() {
    let mut state = team_state(Config::default(), team_snapshot());
    assert!(state.team_sidebar.is_some());

    let mut next = team_snapshot();
    next.revision = 2;
    next.orbit = None;
    state.set_snapshot(Box::new(next));
    assert!(state.team_sidebar.is_none());
}

#[test]
fn collapsed_sidebar_lists_team_panes_in_navigation_order() {
    let mut state = team_state(Config::default(), team_snapshot());
    state.sidebar_collapsed = true;
    state.compose(106, 40).expect("collapsed team frame");

    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_3", "pane_2", "pane_4", "pane_8", "pane_9"],
        "the member without a pane is not numbered"
    );
    assert!(state.hits.team_groups.is_empty());
}

#[test]
fn collapsed_sidebar_with_team_disabled_keeps_agent_order() {
    let mut config = Config::default();
    config.ui.sidebar_team = false;
    let mut state = team_state(config, team_snapshot());
    state.sidebar_collapsed = true;
    state.compose(106, 40).expect("collapsed flat frame");
    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_2", "pane_3", "pane_4", "pane_8", "pane_9"]
    );
}

#[test]
fn keyboard_agent_navigation_follows_team_order() {
    let mut state = team_state(Config::default(), team_snapshot());
    state.compose(106, 40).expect("team sidebar frame");

    let mut index = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusAgent(1)),
        &mut index,
    );
    assert_eq!(focused_pane(&index), Some("pane_3"));

    let mut next = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextAgent),
        &mut next,
    );
    assert_eq!(
        focused_pane(&next),
        Some("pane_3"),
        "lead is focused, next is the first member"
    );

    let mut previous = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::PreviousAgent),
        &mut previous,
    );
    assert_eq!(
        focused_pane(&previous),
        Some("pane_9"),
        "wraps to the last listed agent"
    );
}

#[test]
fn narrow_and_mobile_widths_render_the_team_without_panicking() {
    for (cols, rows) in [(30, 12), (60, 10), (20, 6)] {
        let mut state = team_state(Config::default(), team_snapshot());
        state.compose(cols, rows).expect("narrow team frame");
        state.sidebar_collapsed = true;
        state
            .compose(cols, rows)
            .expect("narrow collapsed team frame");
    }
}

fn team_row_builds(state: &ClientShellState) -> u32 {
    state
        .team_sidebar
        .as_ref()
        .expect("team model")
        .row_builds()
}

#[test]
fn unchanged_frames_reuse_the_cached_team_rows() {
    let mut state = team_state(Config::default(), team_snapshot());
    let first = state.compose(106, 40).expect("first team frame");
    assert_eq!(team_row_builds(&state), 1);
    let first_hits = hit_panes(&state)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    let second = state.compose(106, 40).expect("second team frame");

    assert_eq!(team_row_builds(&state), 1, "no change, no rebuild");
    assert_eq!(sidebar_rows(&first, &state), sidebar_rows(&second, &state));
    assert_eq!(hit_panes(&state), first_hits);
}

#[test]
fn team_rows_rebuild_when_an_input_changes() {
    let mut state = team_state(Config::default(), team_snapshot());
    state.compose(106, 40).expect("team frame");
    assert_eq!(team_row_builds(&state), 1);

    state.collapsed_team_groups.insert("other".into());
    state.compose(106, 40).expect("collapsed other frame");
    assert_eq!(team_row_builds(&state), 2, "collapse state");

    state.compose(120, 40).expect("wider frame");
    let builds = team_row_builds(&state);
    assert!(builds >= 2, "width is part of the key");

    state.config.agent_panel_sort = crate::config::AgentPanelSortConfig::Priority;
    state.compose(120, 40).expect("priority frame");
    assert_eq!(team_row_builds(&state), builds + 1, "sort setting");

    let mut next = team_snapshot();
    next.revision = 2;
    next.agents.pop();
    state.set_snapshot(Box::new(next));
    let mut next_surface = surface();
    next_surface.projection_revision = 2;
    next_surface.surface_revision = 2;
    state.set_pane_surface(next_surface);
    state.compose(120, 40).expect("next snapshot frame");
    assert_eq!(team_row_builds(&state), builds + 2, "new snapshot");
}
