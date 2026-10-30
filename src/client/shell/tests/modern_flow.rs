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
            awaiting_approval: None,
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
            model: None,
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
            assert!(ch.is_ascii(), "non-ASCII {ch:?} in ASCII mode: {row}");
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

// ---- review round 1

#[test]
fn every_lane_header_and_the_flow_label_stay_clickable() {
    let mut state = modern_state(36, flow_snapshot());
    state.compose(130, 120).unwrap();
    let lanes: Vec<_> = state
        .hits
        .chrome_targets
        .iter()
        .filter_map(|(_, target)| match target {
            ChromeTarget::Lane(lane) => Some(*lane),
            ChromeTarget::FlowOpen => None,
        })
        .collect();
    assert_eq!(
        lanes.len(),
        crate::ui::chrome::state::Lane::ALL.len(),
        "{lanes:?}"
    );
    assert!(
        state
            .hits
            .chrome_targets
            .iter()
            .any(|(_, target)| *target == ChromeTarget::FlowOpen),
        "the WORK label opens the flow"
    );
}

#[test]
fn an_open_flow_closes_when_the_flow_view_or_the_modern_style_goes_away() {
    for edit in [
        (|state: &mut ClientShellState| state.config.flow_view = false)
            as fn(&mut ClientShellState),
        |state| {
            state.config.ui_style = crate::config::UiStyleConfig::Classic;
            state.config.refresh_chrome();
        },
    ] {
        let mut state = open_flow(130, 1);
        edit(&mut state);
        state.compose(130, ROWS).unwrap();
        assert!(
            state.overlay.is_none(),
            "the flow stayed open after a live config change"
        );
        assert!(state.hits.flow_popup.is_empty() && state.hits.flow_steps.is_empty());
    }
}

#[test]
fn modern_draw_paths_never_allocate_even_with_wide_graphemes() {
    const FAMILY: &str = "👩‍👩‍👧‍👦👩‍👩‍👧‍👦‍👩‍👩‍👧‍👦";
    let mut projected = flow_snapshot();
    {
        let orbit = projected.orbit.as_mut().unwrap();
        orbit.roles[0].members[0].name = FAMILY.into();
        orbit.active_tasks[1].title = format!("{FAMILY} title");
        let task = &mut orbit.active_tasks[1];
        task.steps[0].member = FAMILY.into();
        task.steps[0].note = Some(format!("note {FAMILY}"));
        task.steps[0].next = Some(format!("next {FAMILY}"));
        task.steps[1].member = FAMILY.into();
    }
    let mut state = modern_state(36, projected);
    let mut outcome = ClientShellInput::default();
    state.open_orbit_flow(&mut outcome);
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = 1;
    }
    state.set_pane_surface(super::modern_headers::bordered_surface(
        (104, 35),
        &[("pane_3", 0, 50), ("pane_2", 50, 50)],
        "pane_3",
    ));
    state.compose(130, ROWS).unwrap();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 130, ROWS));
    let team = state.team_sidebar.as_ref().unwrap();
    let popup = super::super::render::flow_popup(buffer.area).unwrap();
    let inner = super::super::modern::model::flow::inner_of(popup);
    let ClientShellOverlay::OrbitFlow(flow_overlay) = state.overlay.as_ref().unwrap() else {
        unreachable!()
    };
    let orbit = state.snapshot.as_deref().unwrap().orbit.as_ref();
    let draw = super::super::modern::draw::flow::FlowDraw {
        chrome: &state.config.chrome,
        spinner: Some(3),
        now_unix_ms: 10_000_000_000,
        selected_pane: true,
        offered: true,
    };
    let mut steps = super::super::modern::model::flow::FlowHits::default();
    let mut tasks = super::super::modern::model::flow::FlowHits::default();
    let mut hits = ShellHitMap::default();
    hits.team_agents.reserve(64);
    hits.team_groups.reserve(64);
    hits.chrome_targets = Default::default();
    let ctx = super::super::team_sidebar::TeamRenderContext {
        config: &state.config,
        endpoint_id: &state.active_endpoint_id,
        animation: super::super::animation::AnimationFrame::new(
            &state.animations,
            state.config.animations,
            std::time::Instant::now(),
        ),
        focused_pane_id: Some("pane_3"),
        now_unix_ms: 10_000_000_000,
        recent: &state.recent,
        now: std::time::Instant::now(),
    };
    let surface = state.pane_surface.as_ref().unwrap();
    let origin = state.layout(130, ROWS).pane_surface;
    let mut painted = super::super::composition::frame_for_composition(
        &buffer,
        &super::super::composition::FrameCapacity::for_surface(
            surface,
            origin,
            &state.header_copies,
            None,
            true,
        ),
    );
    super::super::blit_pane_surface_modern(
        &mut painted,
        surface,
        origin,
        &state.header_copies,
        team,
        &ctx,
    );
    let activity = state
        .tab_activity
        .values()
        .next()
        .copied()
        .unwrap_or_default();
    let draw_all = |buffer: &mut Buffer,
                    steps: &mut super::super::modern::model::flow::FlowHits,
                    tasks: &mut super::super::modern::model::flow::FlowHits,
                    hits: &mut ShellHitMap,
                    painted: &mut FrameData| {
        steps.clear();
        tasks.clear();
        hits.team_agents.clear();
        hits.team_groups.clear();
        hits.chrome_targets = Default::default();
        super::super::modern::draw::flow::render(
            buffer,
            inner,
            flow_overlay,
            orbit,
            &draw,
            steps,
            tasks,
        );
        for (index, line) in team.rows.lines.iter().enumerate() {
            super::super::modern::draw::team::render_line(
                buffer,
                Rect::new(0, index as u16 % 30, 25, 3),
                index,
                line,
                team,
                &ctx,
                hits,
            );
        }
        for (index, line) in team.rows.lines.iter().enumerate() {
            super::super::modern::draw::lanes::render_line(
                buffer,
                Rect::new(0, index as u16 % 30, 36, 3),
                index,
                line,
                team,
                &ctx,
                hits,
            );
        }
        super::super::modern::draw::tabs::render_suffix(
            buffer,
            Rect::new(60, 0, 20, 1),
            &activity,
            false,
            &state.config,
        );
        super::super::modern::draw::tabs::render_attention(
            buffer,
            Rect::new(100, 0, 20, 1),
            &state.attention_summary,
            &state.config,
        );
        super::super::modern::draw::pane_header::paint(painted, &state.hits.panes, team, &ctx);
    };
    // The first pass sizes the reusable hit lists; the second must not allocate.
    let cold = super::alloc_count::allocations(|| {
        draw_all(&mut buffer, &mut steps, &mut tasks, &mut hits, &mut painted)
    });
    assert_eq!(cold, 0, "cold long-grapheme cached-span draws allocate");
    assert!(
        painted.cells.iter().any(|cell| cell.symbol == "👩‍👩‍👧‍👦‍👩‍👩‍👧‍👦"),
        "header must retain the full cached grapheme"
    );
    let count = super::alloc_count::allocations(|| {
        draw_all(&mut buffer, &mut steps, &mut tasks, &mut hits, &mut painted);
    });
    assert_eq!(count, 0, "modern draw paths allocated {count} times");
}

fn slot_states(state: &ClientShellState) -> Vec<crate::ui::chrome::state::StateKind> {
    flow(state).layout.as_ref().unwrap().states.clone()
}

#[test]
fn each_branch_shows_its_own_live_state_not_the_task_flags() {
    use crate::ui::chrome::state::StateKind;
    // Slots: 0 ada (handed off), 1 bo (active branch), 2 bo2 (done), 3 integrator.
    let mut snapshot = flow_snapshot();
    snapshot.orbit.as_mut().unwrap().active_tasks[1].blocked = true;
    let mut state = modern_state(26, snapshot);
    state.open_orbit_flow(&mut ClientShellInput::default());
    select(&mut state, 0);
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = 1;
    }
    state.compose(130, ROWS).unwrap();
    assert_eq!(
        slot_states(&state),
        [
            StateKind::Done,
            StateKind::Working,
            StateKind::Done,
            StateKind::Queued
        ],
        "a task-level flag belongs to its current worker, not to every branch"
    );

    let mut snapshot = flow_snapshot();
    for agent in snapshot.agents.iter_mut().filter(|a| a.pane_id == "pane_5") {
        agent.agent_status = AgentStatus::Blocked;
    }
    state.set_snapshot(Box::new(snapshot));
    state.compose(130, ROWS).unwrap();
    assert_eq!(
        slot_states(&state)[1],
        StateKind::Blocked,
        "the branch whose agent waits shows it"
    );
    assert_eq!(slot_states(&state)[2], StateKind::Done);
}

#[test]
fn the_flow_is_offered_by_help_the_global_menu_and_the_mobile_menu_only_when_modern() {
    use super::super::flow_view::flow_offered;
    use super::super::global_menu::global_menu_items;
    let state = modern_state(26, flow_snapshot());
    let snapshot = state.snapshot.as_deref().unwrap();
    assert!(flow_offered(&state.config, snapshot));
    let labels = |offer| -> Vec<&'static str> {
        global_menu_items(snapshot, offer)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    };
    assert!(labels(true).contains(&"orbit flow"));
    assert_eq!(
        labels(false),
        ["settings", "keybinds", "reload config", "detach"],
        "the classic menu is unchanged"
    );
    let item = global_menu_items(snapshot, true)
        .into_iter()
        .find(|(label, _)| *label == "orbit flow")
        .unwrap();
    assert_eq!(
        item.1,
        super::super::global_menu::ClientGlobalMenuAction::Binding(
            crate::input::KeybindAction::OrbitFlow
        )
    );

    // Classic, a switched-off flow and a session without a team offer nothing.
    let mut off = modern_state(26, flow_snapshot());
    off.config.flow_view = false;
    assert!(!flow_offered(&off.config, snapshot));
    let mut classic = modern_state(26, flow_snapshot());
    classic.config.ui_style = crate::config::UiStyleConfig::Classic;
    classic.config.refresh_chrome();
    assert!(!flow_offered(&classic.config, snapshot));
    let mut bare = flow_snapshot();
    bare.orbit = None;
    assert!(!flow_offered(&state.config, &bare));
}

#[test]
fn help_lists_the_flow_key_only_while_it_is_offered() {
    for (offered, expected) in [(true, true), (false, false)] {
        let mut state = modern_state(26, flow_snapshot());
        state.config.flow_view = offered;
        state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
            query: TextEditor::default(),
            search_focused: false,
            scroll: 0,
        }));
        let frame = state.compose(130, 50).unwrap();
        let text = super::frame_rows(&frame).join("\n");
        assert_eq!(text.contains("orbit flow"), expected, "{text}");
    }
}

#[test]
fn the_mobile_menu_offers_the_flow_and_opens_it() {
    let mut state = modern_state(26, flow_snapshot());
    state.mode = ClientShellMode::Navigate;
    let frame = state.compose(50, 40).unwrap();
    let text = super::frame_rows(&frame).join("\n");
    assert!(text.contains("orbit flow"), "{text}");
    let target = state
        .hits
        .mobile_targets
        .iter()
        .filter_map(|(rect, target)| match target {
            ClientMobileTarget::Menu(index) => Some((*rect, *index)),
            _ => None,
        })
        .find(|(_, index)| {
            let snapshot = state.snapshot.as_deref().unwrap();
            super::super::global_menu::global_menu_items(snapshot, true)[*index].0 == "orbit flow"
        })
        .expect("a mobile menu target for the flow");
    let click = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: target.0.x,
        row: target.0.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    state.handle_mouse(click, &mut ClientShellInput::default());
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::OrbitFlow(_))),
        "{:?}",
        state.overlay
    );
}

#[test]
fn the_step_model_is_drawn_after_the_kind_and_dropped_before_the_member() {
    let mut snapshot = flow_snapshot();
    let task = &mut snapshot.orbit.as_mut().unwrap().active_tasks[1];
    task.steps[0].model = Some("gpt-5.5-preview-with-a-long-name".into());
    task.steps[1].model = Some("opus-4".into());
    let mut state = modern_state(26, snapshot.clone());
    state.open_orbit_flow(&mut ClientShellInput::default());
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = 1;
    }
    let wide = popup_text(&mut state, 130);
    assert!(
        wide.contains("ada codex · gpt-5.5-preview-with-a-long-name"),
        "{wide}"
    );
    let narrow = popup_text(&mut state, 50);
    assert!(narrow.contains("ada"), "{narrow}");
    assert!(
        !narrow.contains("gpt-5.5"),
        "the model yields first:\n{narrow}"
    );
}

#[test]
fn the_step_model_is_optional_on_the_wire() {
    let step = flow_snapshot().orbit.unwrap().active_tasks[1].steps[0].clone();
    let json = serde_json::to_value(&step).unwrap();
    assert!(json.get("model").is_none(), "{json}");
    let mut with = step.clone();
    with.model = Some("opus".into());
    let json = serde_json::to_value(&with).unwrap();
    assert_eq!(json["model"], "opus");
    let back: crate::protocol::OrbitShellStep = serde_json::from_value(json).unwrap();
    assert_eq!(back.model.as_deref(), Some("opus"));
    let mut old = serde_json::to_value(&step).unwrap();
    old.as_object_mut().unwrap().remove("model");
    let back: crate::protocol::OrbitShellStep = serde_json::from_value(old).unwrap();
    assert_eq!(back.model, None, "an older server omits the field");
}

#[test]
fn a_model_wider_than_the_wire_width_counter_is_omitted_without_overflow() {
    let mut snapshot = flow_snapshot();
    snapshot.orbit.as_mut().unwrap().active_tasks[1].steps[0].model = Some("界".repeat(40_000));
    let mut state = modern_state(26, snapshot);
    state.open_orbit_flow(&mut ClientShellInput::default());
    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
        flow.task = 1;
    }
    assert!(popup_text(&mut state, 130).contains("ada codex"));
}

#[test]
fn classic_writer_retains_long_graphemes_while_modern_writer_stays_inline() {
    use crate::ui::chrome::draw::{put_segment, put_segment_inline};
    let text = "👩‍👩‍👧‍👦";
    let area = Rect::new(0, 0, 8, 1);
    let mut expected = Buffer::empty(area);
    expected.set_stringn(0, 0, text, 8, ratatui::style::Style::default());
    let mut classic = Buffer::empty(area);
    put_segment(
        &mut classic,
        0,
        0,
        8,
        text,
        ratatui::style::Style::default(),
    );
    assert_eq!(classic, expected);
    let mut modern = Buffer::empty(area);
    let count = super::alloc_count::allocations(|| {
        put_segment_inline(&mut modern, 0, 0, 8, text, ratatui::style::Style::default());
    });
    assert_eq!(count, 0);
    assert_eq!(modern[(0, 0)].symbol(), "👩");
}
