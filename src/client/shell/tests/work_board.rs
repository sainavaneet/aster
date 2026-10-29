use super::team_sidebar::{
    click, focused_pane, hit_panes, orbit, pane_hits, row_of, sidebar_rows, team_snapshot,
    team_state,
};
use super::*;
use crate::client::shell::work_board::{age_text, WallClock};
use crate::protocol::{OrbitShellActiveTask, OrbitShellRecentTask};

const NOW_MS: u64 = 10_000_000_000;
const MINUTE_MS: u64 = 60_000;

fn active(
    task_id: &str,
    worker: &str,
    role: Option<&str>,
    state: &str,
    blocked: bool,
    age_ms: u64,
) -> OrbitShellActiveTask {
    OrbitShellActiveTask {
        task_id: task_id.into(),
        worker_terminal_id: worker.into(),
        role: role.map(str::to_owned),
        title: format!("title of {task_id}"),
        state: state.into(),
        created_at_ms: NOW_MS - age_ms,
        blocked,
        awaiting_approval: blocked,
        steps: Vec::new(),
        step_total: None,
        parallel: None,
        steps_omitted: None,
    }
}

fn recent(task_id: &str, outcome: &str, age_ms: u64) -> OrbitShellRecentTask {
    OrbitShellRecentTask {
        task_id: task_id.into(),
        outcome: outcome.into(),
        summary: format!("summary {task_id}"),
        finished_at_ms: NOW_MS - age_ms,
    }
}

fn board_snapshot() -> ClientShellSnapshot {
    let mut projected = team_snapshot();
    let mut view = orbit();
    view.active_tasks = vec![
        active(
            "task-aaaa-11111111",
            "term_pane_2",
            Some("reviewer"),
            "working",
            false,
            2 * MINUTE_MS,
        ),
        active(
            "task-bbbb-22222222",
            "term_pane_3",
            None,
            "blocked",
            true,
            30_000,
        ),
        active(
            "task-cccc-33333333",
            "term_gone",
            None,
            "assigning",
            false,
            5_000,
        ),
    ];
    view.recent_tasks = vec![
        recent("task-dddd-44444444", "completed", MINUTE_MS),
        recent("task-eeee-55555555", "failed", 11 * MINUTE_MS),
    ];
    projected.orbit = Some(view);
    projected
}

fn board_state(config: Config, projected: ClientShellSnapshot) -> ClientShellState {
    let mut state = team_state(config, projected);
    set_clock(&mut state, 0);
    state
}

/// Pins the wall clock so the next frame reads `NOW_MS + offset_ms`.
fn set_clock(state: &mut ClientShellState, offset_ms: u64) {
    state.wall_clock = WallClock::at(std::time::Instant::now(), NOW_MS + offset_ms);
}

fn wide_config() -> Config {
    // These tests pin the classic board; the modern lanes are tested apart.
    let mut config = Config::default();
    config.ui.style = crate::config::UiStyleConfig::Classic;
    config.ui.sidebar_width = 48;
    config.ui.sidebar_max_width = 60;
    config
}

#[test]
fn blocked_tasks_sort_first_and_recent_tasks_follow_the_active_ones() {
    let mut state = board_state(wide_config(), board_snapshot());
    let frame = state.compose(106, 50).expect("work board frame");
    let rows = sidebar_rows(&frame, &state);

    let order = ["work", "22222222", "11111111", "33333333", "44444444"]
        .map(|needle| row_of(&rows, needle));
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "board out of order: {order:?}\n{rows:#?}"
    );
    assert!(
        order[0] > row_of(&rows, "stray-blocked"),
        "board sits below the tree"
    );
    let blocked = &rows[order[1]];
    assert!(blocked.contains("⚑"), "approval marker: {blocked:?}");
    assert!(
        blocked.contains("rev-codex"),
        "worker name when no role: {blocked:?}"
    );
    assert!(rows[order[2]].contains("reviewer"), "role beats worker");
    assert!(rows[order[2]].contains("2m"), "age label");
    assert!(rows[order[1] + 1].contains("title of task-bbbb-22222222"));
    assert!(rows[order[4]].contains("✓") && rows[order[4]].contains("completed"));
}

#[test]
fn header_counts_active_and_blocked_tasks() {
    let mut state = board_state(wide_config(), board_snapshot());
    let frame = state.compose(106, 50).expect("work board frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(
        rows[row_of(&rows, "work")].contains("3 active · 1 blocked"),
        "{rows:#?}"
    );
}

#[test]
fn resolved_tasks_disappear_ten_minutes_after_they_finish() {
    let mut state = board_state(wide_config(), board_snapshot());
    let frame = state.compose(106, 50).expect("work board frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(rows.iter().any(|row| row.contains("44444444")));
    assert!(
        !rows.iter().any(|row| row.contains("55555555")),
        "finished 11 minutes ago"
    );

    set_clock(&mut state, 9 * MINUTE_MS + 1);
    let frame = state.compose(106, 50).expect("aged work board frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(
        !rows.iter().any(|row| row.contains("44444444")),
        "finished 10 minutes ago"
    );
    assert!(rows[row_of(&rows, "work")].contains("3 active"));
}

#[test]
fn a_board_with_nothing_to_show_is_hidden() {
    let mut projected = board_snapshot();
    if let Some(view) = projected.orbit.as_mut() {
        view.active_tasks.clear();
    }
    let mut state = board_state(wide_config(), projected);
    set_clock(&mut state, 20 * MINUTE_MS);
    let frame = state.compose(106, 50).expect("empty work board frame");
    assert!(!sidebar_rows(&frame, &state)
        .iter()
        .any(|row| row.contains("active")));
}

#[test]
fn clicking_a_task_focuses_its_worker_and_an_unresolved_worker_is_inert() {
    let mut state = board_state(wide_config(), board_snapshot());
    let frame = state.compose(106, 50).expect("work board frame");
    let rows = sidebar_rows(&frame, &state);

    let blocked_row = row_of(&rows, "22222222") as u16;
    let hit = pane_hits(&state)
        .into_iter()
        .find(|(rect, _)| rect.y == blocked_row && rect.height == 2)
        .map(|(rect, pane_id)| (rect, pane_id.to_owned()))
        .expect("task hit covers both lines");
    assert_eq!(hit.1, "pane_3");
    assert_eq!(focused_pane(&click(&mut state, hit.0)), Some("pane_3"));

    let detail = Rect::new(hit.0.x + 3, blocked_row + 1, 1, 1);
    assert_eq!(focused_pane(&click(&mut state, detail)), Some("pane_3"));

    let unresolved = row_of(&rows, "33333333") as u16;
    assert!(!pane_hits(&state)
        .iter()
        .any(|(rect, _)| rect.y == unresolved));
    let input = click(&mut state, Rect::new(hit.0.x + 3, unresolved, 1, 1));
    assert!(input.actions.is_empty());

    let finished = row_of(&rows, "44444444") as u16;
    let input = click(&mut state, Rect::new(hit.0.x + 3, finished, 1, 1));
    assert!(
        input.actions.is_empty(),
        "resolved tasks have no worker to focus"
    );
}

#[test]
fn the_work_header_collapses_the_board() {
    let mut state = board_state(wide_config(), board_snapshot());
    let frame = state.compose(106, 50).expect("work board frame");
    let header = row_of(&sidebar_rows(&frame, &state), "work") as u16;
    let rect = state
        .hits
        .team_groups
        .iter()
        .find(|(rect, _)| rect.y == header)
        .expect("work header hit")
        .0;
    assert!(click(&mut state, rect).actions.is_empty());
    let frame = state.compose(106, 50).expect("collapsed work board frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(rows[row_of(&rows, "work")].contains("▸ work"));
    assert!(!rows.iter().any(|row| row.contains("22222222")));
}

#[test]
fn sidebar_work_false_hides_the_board_but_keeps_the_tree() {
    let mut config = wide_config();
    config.ui.sidebar_work = false;
    let mut state = board_state(config, board_snapshot());
    let frame = state.compose(106, 50).expect("tree-only frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(rows.iter().any(|row| row.contains("rev-claude")));
    assert!(!rows
        .iter()
        .any(|row| row.contains("22222222") || row.contains("active")));
    assert_eq!(
        hit_panes(&state),
        ["pane_1", "pane_3", "pane_2", "pane_4", "pane_8", "pane_9"]
    );
}

#[test]
fn age_labels_use_the_largest_whole_unit() {
    let mut buffer = [0u8; 5];
    for (elapsed_ms, expected) in [
        (0, "0s"),
        (59_999, "59s"),
        (60_000, "1m"),
        (3_599_000, "59m"),
        (3_600_000, "1h"),
        (86_400_000, "1d"),
        (u64::MAX, "9999d"),
    ] {
        assert_eq!(age_text(elapsed_ms, &mut buffer), expected);
    }
}

const HOUR_MS: u64 = 60 * MINUTE_MS;

/// The board snapshot as a server whose clock reads `server_now_ms` sends it.
fn stamped_board_snapshot(server_now_ms: u64) -> ClientShellSnapshot {
    let mut projected = board_snapshot();
    if let Some(view) = projected.orbit.as_mut() {
        // Ages are measured on the server's clock, so shift the facts onto it.
        let shift = server_now_ms.wrapping_sub(NOW_MS);
        for task in &mut view.active_tasks {
            task.created_at_ms = task.created_at_ms.wrapping_add(shift);
        }
        for task in &mut view.recent_tasks {
            task.finished_at_ms = task.finished_at_ms.wrapping_add(shift);
        }
        view.server_now_ms = Some(server_now_ms);
    }
    projected
}

#[test]
fn ages_follow_the_server_clock_when_the_client_clock_is_an_hour_behind() {
    let mut state = team_state(wide_config(), stamped_board_snapshot(NOW_MS + HOUR_MS));
    set_clock(&mut state, 0);

    let frame = state.compose(106, 50).expect("skewed work board frame");
    let rows = sidebar_rows(&frame, &state);

    let task = &rows[row_of(&rows, "11111111")];
    assert!(
        task.contains("2m"),
        "created two server minutes ago: {task:?}"
    );
    assert!(
        rows[row_of(&rows, "44444444")].contains("1m"),
        "finished one server minute ago"
    );
    assert!(
        !rows.iter().any(|row| row.contains("55555555")),
        "the ten-minute hide uses the server clock too"
    );
}

#[test]
fn ages_follow_the_server_clock_when_the_client_clock_is_an_hour_ahead() {
    let mut state = team_state(wide_config(), stamped_board_snapshot(NOW_MS - HOUR_MS));
    set_clock(&mut state, 0);

    let frame = state.compose(106, 50).expect("skewed work board frame");
    let rows = sidebar_rows(&frame, &state);

    let task = &rows[row_of(&rows, "11111111")];
    assert!(task.contains("2m"), "not 1h: {task:?}");
    assert!(rows.iter().any(|row| row.contains("44444444")));
}

#[test]
fn ages_fall_back_to_the_client_clock_without_a_server_stamp() {
    let mut state = board_state(wide_config(), board_snapshot());
    assert!(state.orbit_clock.is_none());
    let frame = state.compose(106, 50).expect("work board frame");
    let rows = sidebar_rows(&frame, &state);
    assert!(rows[row_of(&rows, "11111111")].contains("2m"));
}
