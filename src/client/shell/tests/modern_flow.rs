use super::modern_sidebar::{lane_snapshot, modern_state};
use super::*;

/// A snapshot whose first task is a relay that fans out to two codex workers and
/// ends at the integrator, with real pane ids on every step.
pub(super) fn flow_snapshot() -> ClientShellSnapshot {
    let mut projected = lane_snapshot();
    for pane in ["pane_5", "pane_6"] {
        projected.agents.push(super::team_sidebar::agent(
            pane,
            pane,
            AgentStatus::Working,
            9,
        ));
    }
    let orbit = projected.orbit.as_mut().unwrap();
    let step = |index: u16, member: &str, state: &str, pane: Option<&str>| {
        crate::protocol::OrbitShellStep {
            index,
            member: member.into(),
            kind: "codex".into(),
            state: state.into(),
            note: None,
            next: None,
            parallel: None,
            integrate: None,
            started_at_ms: Some(9_999_000_000),
            finished_at_ms: None,
            worker_terminal_id: pane.map(|pane| format!("term_{pane}")),
            pane_id: pane.map(str::to_owned),
        }
    };
    let mut first = step(0, "ada", "handed_off", Some("pane_2"));
    first.note = Some("reproduced the refresh race".into());
    first.next = Some("patch the refresh path, keep 214 tests green".into());
    let mut left = step(1, "bo", "active", Some("pane_5"));
    left.parallel = Some("running".into());
    let mut right = step(2, "bo2", "done", Some("pane_6"));
    right.parallel = Some("running".into());
    let mut integrator = step(3, "Polaris", "pending", None);
    integrator.integrate = Some("waiting".into());
    let task = &mut orbit.active_tasks[1];
    task.steps = vec![first, left, right, integrator];
    task.step_total = Some(4);
    task.parallel = Some(crate::protocol::OrbitShellParallel { done: 1, total: 2 });
    projected
}

const ROWS: u16 = 36;

fn open_flow(cols: u16, task: usize) -> ClientShellState {
    let mut state = modern_state(26, flow_snapshot());
    let mut outcome = ClientShellInput::default();
    state.open_orbit_flow(&mut outcome);
    assert!(outcome.repaint);
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = task;
    }
    state.compose(cols, ROWS).expect("flow frame");
    state
}

fn popup_text(state: &mut ClientShellState, cols: u16) -> String {
    let frame = state.compose(cols, ROWS).unwrap();
    super::frame_rows(&frame).join("\n")
}

fn flow(state: &ClientShellState) -> &ClientOrbitFlowOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::OrbitFlow(flow)) => flow,
        other => panic!("flow overlay expected, got {other:?}"),
    }
}

fn select(state: &mut ClientShellState, slot: usize) {
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.step = slot;
    }
}

#[test]
fn the_flow_shows_lead_role_steps_handoffs_parallel_total_and_integrator() {
    let mut state = open_flow(130, 1);
    let text = popup_text(&mut state, 130);
    for needle in [
        "Polaris ━▶ reviewer",
        "parallel 1/2 done",
        "ada codex",
        "reproduced the refresh race",
        "patch the refresh path, keep 214 tests green",
        "bo",
        "bo2",
        "⊕",
        "waiting",
        "[relay task title",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }
    // Side by side when wide: the two workers share a row.
    let row = text
        .lines()
        .find(|row| row.contains(" bo") && row.contains("bo2"))
        .expect("columns");
    assert!(row.find("bo").unwrap() < row.find("bo2").unwrap());
}

#[test]
fn narrow_terminals_list_parallel_steps_and_keep_member_and_kind() {
    let mut state = open_flow(80, 1);
    let text = popup_text(&mut state, 80);
    assert!(text.contains("├") && text.contains("╰"), "{text}");
    for needle in ["bo codex", "bo2 codex", "parallel 1/2 done"] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
    let mut mobile = open_flow(44, 1);
    let text = popup_text(&mut mobile, 44);
    assert!(
        text.contains("bo2"),
        "member survives mobile width:\n{text}"
    );
}

#[test]
fn two_parallel_workers_of_one_kind_focus_their_own_panes() {
    let mut state = open_flow(130, 1);
    // Slots: 0 ada, 1 bo (pane_5), 2 bo2 (pane_6), 3 the integrator.
    select(&mut state, 1);
    let outcome = state.handle_input_bytes(b"\r");
    assert_eq!(super::team_sidebar::focused_pane(&outcome), Some("pane_5"));
    assert!(state.overlay.is_none(), "focusing closes the view");

    let mut state = open_flow(130, 1);
    select(&mut state, 2);
    let outcome = state.handle_input_bytes(b"\r");
    assert_eq!(super::team_sidebar::focused_pane(&outcome), Some("pane_6"));
}

#[test]
fn a_step_without_a_worker_or_with_a_stopped_one_cannot_be_focused() {
    let mut state = open_flow(130, 1);
    select(&mut state, 3);
    let outcome = state.handle_input_bytes(b"\r");
    assert!(outcome.actions.is_empty(), "the integrator has no pane");
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::OrbitFlow(_))
    ));
    assert!(popup_text(&mut state, 130).contains("no pane for this step"));

    // A worker that stopped leaves the snapshot's agents: its pane is gone.
    let mut stopped = flow_snapshot();
    stopped.agents.retain(|agent| agent.pane_id != "pane_6");
    let mut state = modern_state(26, stopped);
    let mut outcome = ClientShellInput::default();
    state.open_orbit_flow(&mut outcome);
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = 1;
    }
    state.compose(130, ROWS).unwrap();
    select(&mut state, 2);
    assert!(state.flow_slot_pane(2).is_none());
    assert!(state.handle_input_bytes(b"\r").actions.is_empty());
    assert!(
        state.flow_slot_pane(1).is_some(),
        "its sibling still resolves"
    );
}

#[test]
fn keys_move_between_steps_and_tasks_and_close() {
    let mut state = open_flow(130, 1);
    let start = flow(&state).step;
    state.handle_input_bytes(b"j");
    assert_eq!(flow(&state).step, start + 1);
    state.handle_input_bytes(b"k");
    state.handle_input_bytes(b"k");
    state.handle_input_bytes(b"k");
    assert_eq!(flow(&state).step, 0, "clamped at the first step");
    state.handle_input_bytes(b"l");
    assert_eq!(flow(&state).task, 2);
    state.handle_input_bytes(b"h");
    state.handle_input_bytes(b"h");
    state.handle_input_bytes(b"h");
    assert_eq!(flow(&state).task, 0, "clamped at the first task");
    state.handle_input_bytes(b"\x1b");
    assert!(state.overlay.is_none());
}

#[test]
fn mouse_selects_tasks_focuses_steps_and_closes() {
    let mut state = open_flow(130, 1);
    let chip = state
        .hits
        .flow_tasks
        .iter()
        .find(|(_, task)| *task == 2)
        .map(|(rect, _)| *rect)
        .expect("task chip");
    super::team_sidebar::click(&mut state, chip);
    assert_eq!(flow(&state).task, 2);

    let mut state = open_flow(130, 1);
    let (rect, _) = *state
        .hits
        .flow_steps
        .iter()
        .find(|(_, slot)| *slot == 1)
        .unwrap();
    let outcome = super::team_sidebar::click(&mut state, rect);
    assert_eq!(super::team_sidebar::focused_pane(&outcome), Some("pane_5"));

    let mut state = open_flow(130, 1);
    let close = state.hits.overlay_cancel;
    super::team_sidebar::click(&mut state, close);
    assert!(
        state.overlay.is_none(),
        "the close button follows the overlay pattern"
    );
    let mut state = open_flow(130, 1);
    super::team_sidebar::click(&mut state, Rect::new(0, 0, 1, 1));
    assert!(state.overlay.is_none(), "a click outside closes");
}

#[test]
fn the_config_key_turns_the_view_off_and_a_closed_view_costs_nothing() {
    let mut config = Config::default();
    config.ui.flow_view = false;
    let mut off = team_sidebar::team_state(config, flow_snapshot());
    let mut outcome = ClientShellInput::default();
    off.open_orbit_flow(&mut outcome);
    assert!(off.overlay.is_none() && !outcome.repaint);

    let mut closed = modern_state(26, flow_snapshot());
    closed.compose(130, ROWS).unwrap();
    assert!(closed.hits.flow_steps.is_empty() && closed.hits.flow_tasks.is_empty());
    assert!(closed.hits.flow_popup.is_empty());

    let mut none = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    none.set_snapshot(Box::new(snapshot()));
    let mut outcome = ClientShellInput::default();
    none.open_orbit_flow(&mut outcome);
    assert!(none.overlay.is_none(), "no Orbit team, nothing to show");
}

#[test]
fn the_layout_is_rebuilt_only_when_its_key_changes() {
    let mut state = open_flow(130, 1);
    let key = flow(&state).layout.as_ref().unwrap().key;
    state.compose(130, ROWS).unwrap();
    assert_eq!(
        flow(&state).layout.as_ref().unwrap().key,
        key,
        "same frame, same layout"
    );
    state.compose(90, ROWS).unwrap();
    assert_ne!(flow(&state).layout.as_ref().unwrap().key.width, key.width);
    assert!(!flow(&state).layout.as_ref().unwrap().side_by_side);
    state.set_snapshot(Box::new(flow_snapshot()));
    state.set_pane_surface(surface());
    state.compose(90, ROWS).unwrap();
    assert_ne!(flow(&state).layout.as_ref().unwrap().key.serial, key.serial);
}

#[test]
fn ascii_and_motion_modes_keep_the_whole_flow() {
    let mut ascii = open_flow(130, 1);
    ascii.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    ascii.config.refresh_chrome();
    let text = popup_text(&mut ascii, 130);
    let popup = ascii.hits.flow_popup;
    for row in text
        .lines()
        .skip(usize::from(popup.y))
        .take(usize::from(popup.height))
    {
        for ch in row
            .chars()
            .skip(usize::from(popup.x))
            .take(usize::from(popup.width))
        {
            assert!(
                ch.is_ascii() || "┌┐└┘─│".contains(ch),
                "non-ASCII {ch:?} in ASCII mode: {row}"
            );
        }
    }
    assert!(text.contains("Polaris =>"), "{text}");

    use crate::config::UiAnimationsConfig::{Full, Off};
    let frame_with = |mode| {
        let mut config = Config::default();
        config.ui.animations = mode;
        let mut state = team_sidebar::team_state(config, flow_snapshot());
        let mut outcome = ClientShellInput::default();
        state.open_orbit_flow(&mut outcome);
        if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
            flow.task = 1;
        }
        let at = std::time::Instant::now();
        state.animations = super::super::animation::ClientAnimationClock::new(at);
        let frame = state
            .compose_at(130, ROWS, at + std::time::Duration::from_millis(250))
            .unwrap();
        super::frame_rows(&frame).join("\n")
    };
    let off = frame_with(Off);
    assert!(
        off.contains("◐ bo") && off.contains("parallel 1/2 done"),
        "static glyph, full information"
    );
    assert!(
        frame_with(Full).contains("⠼ bo")
            || frame_with(Full).contains("⠹ bo")
            || frame_with(Full).contains("⠸ bo")
    );
}

#[test]
fn classic_keeps_the_flow_binding_inert() {
    let mut config = Config::default();
    config.ui.style = crate::config::UiStyleConfig::Classic;
    let mut state = team_sidebar::team_state(config, flow_snapshot());
    let mut outcome = ClientShellInput::default();
    state.open_orbit_flow(&mut outcome);
    assert!(state.overlay.is_none());
    assert!(!outcome.repaint);
}

fn snapshot_scene(scene: &str) -> ClientShellSnapshot {
    let mut snapshot = flow_snapshot();
    let orbit = snapshot.orbit.as_mut().unwrap();
    let task = &mut orbit.active_tasks[1];
    match scene {
        "relay" => {
            task.parallel = None;
            task.steps.retain(|step| step.parallel.is_none());
        }
        "parallel-four" => {
            let mut third = task.steps[1].clone();
            third.index = 4;
            third.member = "third".into();
            let mut fourth = third.clone();
            fourth.index = 5;
            fourth.member = "fourth".into();
            task.steps.insert(3, third);
            task.steps.insert(4, fourth);
            task.parallel = Some(crate::protocol::OrbitShellParallel { done: 1, total: 4 });
        }
        "integrating-notes" => {
            task.steps[0].note = Some("界 café 👩‍💻 a very long explanation repeated to exercise clipping at every width\nsecond line".into());
            task.steps[3].state = "active".into();
            task.steps[3].integrate = Some("running".into());
        }
        "approval-blocked" | "blocked" => {
            task.awaiting_approval = scene == "approval-blocked";
            task.blocked = true;
            task.worker_terminal_id = task.steps[1].worker_terminal_id.clone().unwrap();
        }
        "empty" => orbit.active_tasks.clear(),
        _ => {}
    }
    snapshot
}

#[test]
fn modern_flow_snapshot_matrix() {
    let mut actual = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for glyphs in ["unicode", "ascii"] {
                for scene in [
                    "relay",
                    "parallel-two",
                    "parallel-four",
                    "integrating-notes",
                    "approval-blocked",
                    "blocked",
                    "empty",
                ] {
                    for cols in [44, 80, 120, 200] {
                        let config: Config = toml::from_str(&format!("[theme]\nname='{theme}'\n[ui]\nanimations='{motion}'\nglyphs='{glyphs}'\n")).unwrap();
                        let mut state = team_sidebar::team_state(config, snapshot_scene(scene));
                        let base = std::time::Instant::now();
                        state.animations = super::super::animation::ClientAnimationClock::new(base);
                        state.wall_clock =
                            super::super::work_board::WallClock::at(base, 10_000_000_000);
                        state.orbit_clock = None;
                        state.open_orbit_flow(&mut ClientShellInput::default());
                        if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
                            flow.task = if scene == "empty" { 0 } else { 1 };
                        }
                        let frame = state
                            .compose_at(cols, ROWS, base + std::time::Duration::from_millis(160))
                            .unwrap();
                        actual.push_str(&format!("=== {theme}/{motion}/{glyphs}/{scene}/{cols}\n"));
                        let mut hash = 0xcbf29ce484222325u64;
                        for row in frame.cells.chunks(usize::from(cols)) {
                            for cell in row {
                                actual.push_str(&cell.symbol);
                                for byte in format!(
                                    "{}:{}:{}:{}:{:?};",
                                    cell.fg, cell.bg, cell.modifier, cell.skip, cell.hyperlink
                                )
                                .bytes()
                                {
                                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                                }
                            }
                            actual.push_str("|\n");
                        }
                        actual.push_str(&format!("style={hash:016x}\n"));
                    }
                }
            }
        }
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/modern-flow-phase4.txt"
    );
    if std::env::var_os("ASTER_CAPTURE_MODERN_FLOW").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}

#[test]
fn clipped_flow_hits_stay_inside_the_popup_and_reach_the_last_step() {
    for cols in [44, 80, 120] {
        for rows in [12, 18, 36] {
            let mut state = open_flow(cols, 1);
            select(&mut state, 3);
            state.compose(cols, rows).unwrap();
            let popup = state.hits.flow_popup;
            for (rect, _) in state
                .hits
                .flow_steps
                .iter()
                .chain(state.hits.flow_tasks.iter())
            {
                assert_eq!(rect.intersection(popup), *rect);
            }
            if popup.height >= 10 {
                assert!(state.hits.flow_steps.iter().any(|(_, slot)| *slot == 3));
            }
        }
    }
}

#[test]
fn a_snapshot_change_cannot_focus_a_step_from_the_previous_layout() {
    let mut state = open_flow(120, 1);
    let mut changed = flow_snapshot();
    changed.orbit.as_mut().unwrap().active_tasks[1]
        .steps
        .swap(1, 2);
    state.set_snapshot(Box::new(changed));
    select(&mut state, 1);
    let outcome = state.handle_input_bytes(b"\r");
    assert!(outcome.actions.is_empty());
    assert!(state.overlay.is_some());
}

#[test]
fn flow_hit_storage_is_bounded_by_the_wire_projection_cap() {
    let mut hits = super::super::modern::model::flow::FlowHits::default();
    for index in 0..40 {
        hits.push((Rect::new(1, 2, 3, 1), index));
    }
    assert_eq!(hits.iter().count(), 32);
    assert_eq!(hits.iter().last().unwrap().1, 31);
}

#[test]
fn clipped_two_line_parallel_rows_keep_the_selected_hit_target() {
    for rows in [12, 13, 14, 16, 18] {
        let mut state = open_flow(120, 1);
        select(&mut state, 1);
        state.compose(120, rows).unwrap();
        assert!(
            state.hits.flow_steps.iter().any(|(_, slot)| *slot == 1),
            "height {rows}"
        );
    }
}

#[test]
fn snapshot_reordering_preserves_selected_task_and_original_step() {
    let mut state = open_flow(120, 1);
    select(&mut state, 2);
    let mut changed = flow_snapshot();
    let tasks = &mut changed.orbit.as_mut().unwrap().active_tasks;
    tasks[1].steps.remove(0);
    tasks.swap(0, 1);
    state.set_snapshot(Box::new(changed));
    state.set_pane_surface(surface());
    state.compose(120, ROWS).unwrap();
    assert_eq!(flow(&state).task, 0);
    assert_eq!(state.flow_slot_pane(flow(&state).step), Some("pane_6"));
}
