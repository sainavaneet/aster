use super::team_sidebar::{row_of, sidebar_rows, team_snapshot, team_state};
use super::*;
use crate::protocol::{OrbitShellActiveTask, OrbitShellParallel, OrbitShellStep};

const NOW_MS: u64 = 10_000_000_000;

fn step(index: u16, member: &str, kind: &str, state: &str) -> OrbitShellStep {
    OrbitShellStep {
        index,
        member: member.into(),
        kind: kind.into(),
        state: state.into(),
        note: None,
        next: None,
        parallel: None,
        integrate: None,
        started_at_ms: None,
        finished_at_ms: None,
        worker_terminal_id: None,
        pane_id: None,
    }
}

fn task(id: &str, worker: &str, state: &str, age_s: u64) -> OrbitShellActiveTask {
    OrbitShellActiveTask {
        task_id: format!("task-{id}"),
        worker_terminal_id: worker.into(),
        role: Some("reviewer".into()),
        title: format!("{id} task title"),
        state: state.into(),
        created_at_ms: NOW_MS - age_s * 1000,
        blocked: false,
        awaiting_approval: false,
        steps: Vec::new(),
        step_total: None,
        parallel: None,
        steps_omitted: None,
    }
}

/// A scene with every lane: approval, relay, parallel, integrating, queued, done.
pub(super) fn lane_snapshot() -> ClientShellSnapshot {
    let mut projected = team_snapshot();
    let orbit = projected.orbit.as_mut().unwrap();
    orbit.roles[0].members[0].model = Some("gpt-5".into());
    orbit.roles[0].members[1].model = Some("opus".into());
    let mut approval = task("approve", "term_pane_3", "blocked", 240);
    approval.blocked = true;
    approval.awaiting_approval = true;
    let mut relay = task("relay", "term_pane_2", "working", 720);
    relay.steps = vec![
        step(0, "rev-codex", "codex", "handed_off"),
        step(1, "rev-claude", "claude", "active"),
        step(2, "rev-gone", "claude", "pending"),
    ];
    relay.step_total = Some(3);
    let mut fanned = task("fanout", "term_pane_4", "working", 180);
    let branch = |index, state: &str| {
        let mut branch = step(index, "probe", "claude", state);
        branch.parallel = Some("running".into());
        branch
    };
    fanned.steps = vec![branch(1, "done"), branch(2, "active")];
    fanned.parallel = Some(OrbitShellParallel { done: 1, total: 2 });
    let mut merging = task("merge", "term_pane_4", "working", 60);
    let mut integrator = step(3, "probe", "claude", "active");
    integrator.integrate = Some("applying".into());
    merging.steps = vec![integrator];
    merging.parallel = Some(OrbitShellParallel { done: 2, total: 2 });
    merging.step_total = Some(1);
    let queued = task("queued", "term_pane_4", "assigning", 20);
    orbit.active_tasks = vec![approval, relay, fanned, merging, queued];
    orbit.recent_tasks = vec![crate::protocol::OrbitShellRecentTask {
        task_id: "task-done".into(),
        outcome: "completed".into(),
        summary: "bumped dependencies".into(),
        finished_at_ms: NOW_MS - 90_000,
    }];
    projected
}

pub(super) fn modern_state(cols: u16, snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut config = Config::default();
    config.ui.sidebar_width = cols;
    config.ui.sidebar_max_width = cols.max(36);
    let mut state = team_state(config, snapshot);
    let now = std::time::Instant::now();
    state.animations = super::super::animation::ClientAnimationClock::new(now);
    state.wall_clock = super::super::work_board::WallClock::at(now, NOW_MS);
    state.orbit_clock = None;
    state
}

const TALL: u16 = 120;

fn rows_at(width: u16) -> (ClientShellState, Vec<String>) {
    let mut state = modern_state(width, lane_snapshot());
    let frame = state.compose(106, TALL).expect("modern sidebar frame");
    let rows = sidebar_rows(&frame, &state);
    (state, rows)
}

fn builds(state: &ClientShellState) -> u32 {
    state
        .team_sidebar
        .as_ref()
        .expect("team model")
        .row_builds()
}

#[test]
fn lanes_show_needs_you_working_queued_and_done_with_progress() {
    let (_, rows) = rows_at(36);
    let at = |needle: &str| row_of(&rows, needle);
    assert!(at("needs you") < at("working"));
    assert!(at("working") < at("queued"));
    assert!(at("queued") < at("done"));
    for needle in [
        "step 2/3",
        "parallel 1/2",
        "integrating",
        "⚑ reviewer",
        "approve task title",
    ] {
        assert!(rows.iter().any(|row| row.contains(needle)), "{needle}");
    }
    let header = &rows[at("WORK")];
    assert!(
        header.contains("▲1") && header.contains("◐3") && header.contains("✓1"),
        "{header}"
    );
    // Models appear on member rows at the wide tier, with the open task below.
    assert!(rows.iter().any(|row| row.contains("claude·opus")));
    assert!(rows.iter().any(|row| row.contains("codex·gpt-5")));
}

#[test]
fn narrow_sidebars_drop_detail_rows_instead_of_wrapping() {
    let (_, rows) = rows_at(20);
    assert!(rows.iter().any(|row| row.contains("needs you")));
    assert!(!rows.iter().any(|row| row.contains("approve task title")));
    assert!(!rows.iter().any(|row| row.contains("step 2/3")));
}

#[test]
fn lane_headers_are_typed_targets_that_collapse_the_lane() {
    let (mut state, rows) = rows_at(36);
    assert!(state.hits.chrome_targets.len() >= 4);
    let header_y = row_of(&rows, "working") as u16;
    let rect = state
        .hits
        .chrome_targets
        .iter()
        .find(|(rect, _)| rect.y == header_y)
        .map(|(rect, _)| *rect)
        .expect("working lane target");
    let before = builds(&state);
    let outcome = super::team_sidebar::click(&mut state, rect);
    assert!(outcome.actions.is_empty(), "a lane toggle sends nothing");
    assert!(state
        .collapsed_lanes
        .contains(crate::ui::chrome::state::Lane::Working));
    let frame = state.compose(106, TALL).unwrap();
    let rows = sidebar_rows(&frame, &state);
    assert!(!rows.iter().any(|row| row.contains("step 2/3")));
    assert!(rows.iter().any(|row| row.contains("approve task title")));
    assert_eq!(builds(&state), before + 1, "one rebuild for the toggle");
    // Role names cannot collide with lane state: a role called `working` is a group.
    assert!(state.collapsed_team_groups.is_empty());
}

#[test]
fn clicking_a_task_card_focuses_its_worker() {
    let (mut state, rows) = rows_at(36);
    let y = row_of(&rows, "approve task title") as u16 - 1;
    let rect = state
        .hits
        .team_agents
        .iter()
        .map(|(rect, _)| *rect)
        .find(|rect| rect.y <= y && y < rect.bottom())
        .expect("card hit");
    let outcome = super::team_sidebar::click(&mut state, rect);
    assert_eq!(super::team_sidebar::focused_pane(&outcome), Some("pane_3"));
}

#[test]
fn rows_rebuild_only_when_a_key_input_changes() {
    let mut state = modern_state(36, lane_snapshot());
    state.compose(106, TALL).unwrap();
    let first = builds(&state);
    state.compose(106, TALL).unwrap();
    assert_eq!(builds(&state), first, "identical frames reuse the rows");
    state.compose(106, TALL - 10).unwrap();
    assert_eq!(builds(&state), first + 1, "a new section height rebuilds");
    state.compose(106, TALL - 10).unwrap();
    assert_eq!(builds(&state), first + 1);
    state.config.ui_style = crate::config::UiStyleConfig::Classic;
    state.config.refresh_chrome();
    state.compose(106, TALL - 10).unwrap();
    assert_eq!(builds(&state), first + 2, "switching style rebuilds");
    assert!(
        state.hits.chrome_targets.is_empty(),
        "classic has no typed targets"
    );
}

#[test]
fn a_tall_section_gets_spacers_and_a_short_one_truncates_done() {
    let (_, rows) = rows_at(36);
    let work = row_of(&rows, "WORK");
    assert!(
        rows[work - 1].trim().is_empty(),
        "a blank row precedes WORK"
    );
    // Somewhere between "everything fits" and "even the lanes overflow" the only
    // rows that give way are the resolved tasks, and the header says how many.
    let mut truncated = false;
    for height in 40..90 {
        let mut state = modern_state(36, lane_snapshot());
        state.compose(106, height).unwrap();
        let team = state.team_sidebar.as_ref().unwrap();
        let hidden = team.rows.lines.iter().find_map(|line| match line {
            super::super::team_rows::TeamLineRef::LaneHeader {
                lane: crate::ui::chrome::state::Lane::Done,
                hidden,
                ..
            } => Some(*hidden),
            _ => None,
        });
        let done_rows = team
            .rows
            .lines
            .iter()
            .filter(|line| matches!(line, super::super::team_rows::TeamLineRef::LaneDone(_)))
            .count();
        assert_eq!(
            done_rows + usize::from(hidden.unwrap_or(0)),
            1,
            "height {height}"
        );
        truncated |= hidden.is_some_and(|hidden| hidden > 0);
    }
    assert!(truncated, "some height drops the done row");
}

#[test]
fn keyboard_reveal_uses_the_cached_modern_rows() {
    let (state, rows) = rows_at(36);
    let team = state.team_sidebar.as_ref().unwrap();
    let index = team
        .row_index_of(
            state.snapshot.as_deref().unwrap(),
            state.config.agent_panel_sort,
            &state.collapsed_team_groups,
            "pane_3",
        )
        .expect("member row");
    assert!(matches!(
        team.rows.lines[index],
        super::super::team_rows::TeamLineRef::Member { .. }
    ));
    assert!(rows.iter().any(|row| row.contains("rev-codex")));
}

fn snapshot_matrix() -> String {
    let mut out = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for glyphs in ["unicode", "ascii"] {
                for (scene, width, height, sidebar) in [
                    ("desktop-80", 80, 120, 36),
                    ("desktop-120", 120, 120, 36),
                    ("mobile", 44, 30, 26),
                    ("collapsed", 80, 40, 36),
                ] {
                    let config: Config = toml::from_str(&format!(
                        "[theme]\nname = '{theme}'\n[ui]\nanimations = '{motion}'\nglyphs = '{glyphs}'\nsidebar_width = {sidebar}\nsidebar_max_width = 48\n"
                    )).unwrap();
                    let mut projected = lane_snapshot();
                    let mut needs_me = task("needs-me", "term_pane_3", "idle", 500);
                    needs_me.blocked = true;
                    needs_me.title = "needs-me: review 界 changes".into();
                    projected
                        .orbit
                        .as_mut()
                        .unwrap()
                        .active_tasks
                        .push(needs_me);
                    let mut state = team_state(config, projected.clone());
                    let base = std::time::Instant::now();
                    state.animations = super::super::animation::ClientAnimationClock::new(base);
                    state.animations.observe_snapshot(
                        &ClientEndpointId::Local,
                        Some(1),
                        &projected,
                        base,
                    );
                    state.wall_clock = super::super::work_board::WallClock::at(base, NOW_MS);
                    state.orbit_clock = None;
                    state.sidebar_collapsed = scene == "collapsed";
                    let frame = state
                        .compose_at(width, height, base + std::time::Duration::from_millis(360))
                        .unwrap();
                    if scene.starts_with("desktop") {
                        let text = frame_rows(&frame).join("\n");
                        for needle in [
                            "needs you",
                            "step 2/3",
                            "parallel 1/2",
                            "integrating",
                            "needs-me:",
                        ] {
                            assert!(text.contains(needle), "{scene} {needle}");
                        }
                        assert!(state.hits.chrome_targets.len() >= 4);
                    } else {
                        assert!(state.hits.chrome_targets.is_empty());
                    }
                    out.push_str(&format!("=== {theme}/{motion}/{glyphs}/{scene}\n"));
                    let mut hash = 0xcbf29ce484222325u64;
                    for row in frame.cells.chunks(width as usize) {
                        for cell in row {
                            out.push_str(&cell.symbol);
                            for byte in format!(
                                "{}:{}:{}:{}:{:?};",
                                cell.fg, cell.bg, cell.modifier, cell.skip, cell.hyperlink
                            )
                            .bytes()
                            {
                                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                            }
                        }
                        out.push_str("|\n");
                    }
                    out.push_str(&format!("style={hash:016x}\n"));
                }
            }
        }
    }
    out
}

#[test]
fn modern_sidebar_snapshots() {
    let actual = snapshot_matrix();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/modern-sidebar-phase2.txt"
    );
    if std::env::var_os("ASTER_CAPTURE_MODERN_SIDEBAR").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}

#[test]
fn lane_hit_edges_and_mouse_disable_do_not_leave_stale_targets() {
    let (mut state, _) = rows_at(36);
    let targets: Vec<_> = state.hits.chrome_targets.iter().copied().collect();
    for (rect, target) in targets {
        for x in [rect.x, rect.right() - 1] {
            let hit = state
                .hits
                .chrome_targets
                .iter()
                .find(|(area, _)| super::super::contains(*area, (x, rect.y)));
            assert_eq!(hit.map(|(_, kind)| *kind), Some(target));
        }
        assert!(!super::super::contains(rect, (rect.right(), rect.y)));
        assert!(!super::super::contains(rect, (rect.x, rect.bottom())));
    }
    state.config.mouse_capture = false;
    state.compose(106, TALL).unwrap();
    assert!(state.hits.chrome_targets.is_empty());
    state.config.mouse_capture = true;
    state.compose(106, TALL).unwrap();
    assert!(!state.hits.chrome_targets.is_empty());
    state.set_snapshot(Box::new(team_snapshot()));
    assert!(
        state.hits.chrome_targets.is_empty(),
        "replaced projection clears old lane hits"
    );
}

#[test]
fn glyph_and_density_changes_invalidate_once_but_theme_only_does_not() {
    let mut state = modern_state(22, lane_snapshot());
    state.compose(106, TALL).unwrap();
    let mut count = builds(&state);
    for width in [23, 32, 33] {
        state.sidebar_width = width;
        state.compose(106, TALL).unwrap();
        count += 1;
        assert_eq!(builds(&state), count);
        state.compose(106, TALL).unwrap();
        assert_eq!(builds(&state), count);
    }
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    state.compose(106, TALL).unwrap();
    count += 1;
    assert_eq!(builds(&state), count);
    state
        .config
        .set_palette(Palette::from_name("catppuccin-latte").unwrap());
    state.compose(106, TALL).unwrap();
    assert_eq!(builds(&state), count, "rows store no theme-dependent data");
}

#[test]
fn expired_done_tasks_leave_both_rows_and_summary_on_the_deadline() {
    let mut state = modern_state(36, lane_snapshot());
    let base = std::time::Instant::now();
    state.wall_clock = super::super::work_board::WallClock::at(base, NOW_MS);
    state.compose_at(106, TALL, base).unwrap();
    let before = builds(&state);
    assert_eq!(state.team_sidebar.as_ref().unwrap().rows.visible_done, 1);
    let at = base + std::time::Duration::from_millis(510_000);
    let frame = state.compose_at(106, TALL, at).unwrap();
    assert_eq!(builds(&state), before + 1);
    assert_eq!(state.team_sidebar.as_ref().unwrap().rows.visible_done, 0);
    let rows = sidebar_rows(&frame, &state);
    assert!(!rows.iter().any(|row| row.contains("bumped dependencies")));
    assert!(!rows[row_of(&rows, "WORK")].contains("✓1"));
    state.compose_at(106, TALL, at).unwrap();
    assert_eq!(builds(&state), before + 1);
}

#[test]
fn task_age_bars_cross_each_bucket_without_rebuilding_rows() {
    for (seconds, expected) in [
        (0, "───"),
        (60, "━──"),
        (300, "━━─"),
        (900, "━━━"),
        (3600, "━━━"),
    ] {
        let mut projected = lane_snapshot();
        let mut aged = task("a", "term_pane_2", "working", seconds);
        aged.role = Some("r".into());
        projected.orbit.as_mut().unwrap().active_tasks = vec![aged];
        let mut state = modern_state(36, projected);
        let base = std::time::Instant::now();
        state.wall_clock = super::super::work_board::WallClock::at(base, NOW_MS);
        let frame = state.compose_at(106, TALL, base).unwrap();
        let team = state.team_sidebar.as_ref().unwrap();
        let card = team
            .rows
            .lines
            .iter()
            .position(|line| matches!(line, super::super::team_rows::TeamLineRef::LaneTask { .. }))
            .unwrap();
        let rect = state
            .hits
            .team_agents
            .iter()
            .find(|(_, index)| *index == card)
            .unwrap()
            .0;
        let text = &frame_rows(&frame)[usize::from(rect.y)];
        assert!(text.contains(expected), "{seconds}: {text}");
        let before = builds(&state);
        state
            .compose_at(106, TALL, base + std::time::Duration::from_secs(61))
            .unwrap();
        assert_eq!(
            builds(&state),
            before,
            "age updates are scalar draw-time work"
        );
    }
}

// ---- review round 1: one approval-aware state model, essential text first,
// glyph-set separators, and lanes that actually draw.

/// A working agent whose task waits on an approval: the agent screen alone says
/// `working`, the approval-aware model says `needs you`.
fn approval_snapshot() -> ClientShellSnapshot {
    let mut projected = lane_snapshot();
    for agent in &mut projected.agents {
        if agent.pane_id == "pane_3" {
            agent.agent_status = AgentStatus::Working;
        }
    }
    projected.orbit.as_mut().unwrap().roles[0].members[0].state = "working".into();
    projected
}

fn frame_text(frame: &FrameData) -> Vec<String> {
    super::frame_rows(frame)
}

#[test]
fn collapsed_sidebar_keeps_the_approval_marker() {
    let mut state = modern_state(26, approval_snapshot());
    state.sidebar_collapsed = true;
    let frame = state.compose(106, 40).unwrap();
    let rows = frame_text(&frame);
    let rect = state
        .hits
        .agents
        .iter()
        .find(|(_, pane_id)| pane_id == "pane_3")
        .map(|(rect, _)| *rect)
        .expect("collapsed entry for the approving worker");
    let row: String = rows[usize::from(rect.y)]
        .chars()
        .skip(usize::from(rect.x))
        .take(6)
        .collect();
    assert!(
        row.contains('⚑'),
        "collapsed row lost the approval marker: {row:?}"
    );
    // A plain working agent next to it keeps the working glyph.
    let plain = state
        .hits
        .agents
        .iter()
        .find(|(_, pane_id)| pane_id == "pane_2")
        .map(|(rect, _)| *rect)
        .unwrap();
    let plain_row: String = rows[usize::from(plain.y)]
        .chars()
        .skip(usize::from(plain.x))
        .take(6)
        .collect();
    assert!(!plain_row.contains('⚑'), "{plain_row:?}");
}

#[test]
fn mobile_switcher_keeps_the_approval_marker() {
    let mut state = modern_state(26, approval_snapshot());
    state.compose(44, 30).unwrap();
    let switch = state.hits.mobile_switch;
    state.handle_raw_events(vec![super::team_sidebar::click_event(switch)]);
    let frame = state.compose(44, 40).expect("mobile switcher");
    let text = frame_text(&frame).join("\n");
    assert!(
        text.contains('⚑'),
        "mobile agent list lost the approval marker:\n{text}"
    );
}

fn long_names_snapshot() -> ClientShellSnapshot {
    let mut projected = team_snapshot();
    let orbit = projected.orbit.as_mut().unwrap();
    orbit.roles[0].name = "extraordinarily-long-role-name".into();
    orbit.roles[0].members[1].name = "a-member-with-a-very-long-name".into();
    let mut relay = task("relay", "term_pane_2", "working", 720);
    relay.role = Some("extraordinarily-long-role-name".into());
    relay.title = "a very long relay task title that cannot fit".into();
    relay.steps = (0..8u16)
        .map(|index| {
            let state = match index {
                0..=5 => "handed_off",
                6 => "active",
                _ => "pending",
            };
            step(index, "rev-claude", "claude", state)
        })
        .collect();
    relay.step_total = Some(8);
    let mut fanned = task("fanout", "term_pane_4", "working", 180);
    fanned.role = Some("extraordinarily-long-role-name".into());
    let branch = |index: u16, state: &str| {
        let mut branch = step(index, "probe", "claude", state);
        branch.parallel = Some("running".into());
        branch
    };
    fanned.steps = (1..=4)
        .map(|index| branch(index, if index < 3 { "done" } else { "active" }))
        .collect();
    fanned.parallel = Some(OrbitShellParallel { done: 2, total: 4 });
    orbit.active_tasks = vec![relay, fanned];
    orbit.recent_tasks.clear();
    projected
        .agents
        .retain(|agent| agent.pane_id != "pane_8" && agent.pane_id != "pane_9");
    projected
}

#[test]
fn essential_text_survives_narrow_widths_and_long_names() {
    for width in [23u16, 26, 32, 36] {
        let mut state = modern_state(width, long_names_snapshot());
        let frame = state.compose(106, TALL).unwrap();
        let rows = sidebar_rows(&frame, &state);
        let joined = rows.join("\n");
        assert!(
            joined.contains("step 7/8"),
            "width {width}: relay progress clipped\n{joined}"
        );
        assert!(
            joined.contains("parallel 2/4"),
            "width {width}: parallel progress clipped\n{joined}"
        );
        let card = rows
            .iter()
            .find(|row| row.contains("extraordin") && row.trim_end().ends_with("12m"))
            .unwrap_or_else(|| {
                panic!("width {width}: the age was suppressed by a long role name\n{joined}")
            });
        assert!(card.contains("12m"));
    }
    // The narrow tier drops detail rows but still shows who and how long, and a
    // long role name still leaves its state roll-up.
    for width in [18u16, 22] {
        let mut state = modern_state(width, long_names_snapshot());
        let frame = state.compose(80, TALL).unwrap();
        let rows = sidebar_rows(&frame, &state);
        assert!(rows.iter().any(|row| row.contains("12m")), "{rows:#?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("extra") && row.contains('▲')),
            "width {width}: roll-up lost to a long role name {:#?}",
            rows.iter()
                .filter(|row| row.contains('▌'))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn ascii_mode_draws_only_ascii_in_the_modern_team_and_work_rows() {
    let mut projected = lane_snapshot();
    projected
        .agents
        .retain(|agent| agent.pane_id != "pane_8" && agent.pane_id != "pane_9");
    let mut config = Config::default();
    config.ui.glyphs = crate::config::UiGlyphsConfig::Ascii;
    config.ui.sidebar_width = 36;
    config.ui.sidebar_max_width = 36;
    let mut state = team_state(config, projected);
    let now = std::time::Instant::now();
    state.animations = super::super::animation::ClientAnimationClock::new(now);
    state.wall_clock = super::super::work_board::WallClock::at(now, NOW_MS);
    state.orbit_clock = None;
    let frame = state.compose(106, TALL).unwrap();
    let rows = sidebar_rows(&frame, &state);
    let start = row_of(&rows, "grouped");
    // The last row is the shared collapse toggle, drawn by the classic chrome.
    for row in &rows[start..rows.len() - 1] {
        assert!(row.is_ascii(), "non-ASCII in ASCII mode: {row:?}");
    }
    assert!(
        rows.iter().any(|row| row.contains("claude.opus")),
        "{rows:#?}"
    );
}

#[test]
fn switching_glyphs_rebuilds_the_rows_and_redraws_without_stale_unicode() {
    let mut state = modern_state(36, lane_snapshot());
    state.compose(106, TALL).unwrap();
    let first = builds(&state);
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    let frame = state.compose(106, TALL).unwrap();
    assert_eq!(builds(&state), first + 1);
    let rows = sidebar_rows(&frame, &state);
    assert!(rows.iter().any(|row| row.contains("claude.opus")));
    assert!(!rows
        .iter()
        .any(|row| row.contains('·') && row.contains("claude")));
}

#[test]
fn large_relay_counts_keep_progress_when_only_one_or_two_node_cells_fit() {
    let mut projected = lane_snapshot();
    let mut relay = task("large", "term_pane_2", "working", 120);
    relay.steps = vec![step(12_344, "rev-claude", "claude", "active")];
    relay.step_total = Some(u16::MAX);
    projected.orbit.as_mut().unwrap().active_tasks = vec![relay];
    for width in [23, 24, 25] {
        let mut state = modern_state(width, projected.clone());
        let frame = state.compose(106, TALL).unwrap();
        assert!(
            sidebar_rows(&frame, &state)
                .join("\n")
                .contains("step 12345/65535"),
            "width {width}"
        );
    }
}

#[test]
fn approval_cache_refreshes_and_keeps_endpoint_pane_ids_separate() {
    use super::super::modern::model::team::{pane_state, PaneAttention};
    use crate::ui::chrome::state::StateKind;
    let approved = approval_snapshot();
    let waiting = PaneAttention::build(approved.orbit.as_ref());
    let mut other = approved.orbit.clone().unwrap();
    other.active_tasks.clear();
    let other_cache = PaneAttention::build(Some(&other));
    assert_eq!(
        pane_state(Some(&waiting), "pane_3", AgentStatus::Working),
        StateKind::Approval
    );
    assert_eq!(
        pane_state(Some(&other_cache), "pane_3", AgentStatus::Working),
        StateKind::Working
    );
    assert_eq!(
        pane_state(Some(&waiting), "unrelated", AgentStatus::Idle),
        StateKind::Idle
    );
    let mut presentation = super::super::endpoint_agent_state::EndpointAgentPresentation::default();
    let mut endpoint_snapshot = approved.clone();
    presentation.project_snapshot(&mut endpoint_snapshot);
    assert_eq!(
        pane_state(
            Some(&presentation.attention),
            "pane_3",
            AgentStatus::Working
        ),
        StateKind::Approval
    );
    endpoint_snapshot.orbit = None;
    endpoint_snapshot.boot_id = "new-boot".into();
    presentation.project_snapshot(&mut endpoint_snapshot);
    assert_eq!(
        pane_state(
            Some(&presentation.attention),
            "pane_3",
            AgentStatus::Working
        ),
        StateKind::Working
    );
    let mut state = modern_state(26, approved);
    state.sidebar_collapsed = true;
    state.compose(106, 40).unwrap();
    let mut next = state.snapshot.as_deref().unwrap().clone();
    next.orbit.as_mut().unwrap().active_tasks.clear();
    state.set_snapshot(Box::new(next));
    let frame = state.compose(106, 40).unwrap();
    let rect = state
        .hits
        .agents
        .iter()
        .find(|(_, pane)| pane == "pane_3")
        .unwrap()
        .0;
    assert!(!frame_rows(&frame)[usize::from(rect.y)].contains('⚑'));
}
