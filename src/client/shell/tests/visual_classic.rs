use super::*;
use std::time::{Duration, Instant};

// The fixture was captured from d870c4a8 (the tree before the visual
// foundation), checked out with `git archive` into a scratch directory outside
// this checkout and rendered by `render_matrix` below, unchanged. It must keep
// matching while modern surfaces land: `ui.style = "classic"` is pixel-identical
// to that tree.

struct Scene {
    name: &'static str,
    cols: u16,
    rows: u16,
    /// Runs the scene at every animation mode, not only `full`.
    motions: bool,
}

const fn scene(name: &'static str, cols: u16, rows: u16, motions: bool) -> Scene {
    Scene {
        name,
        cols,
        rows,
        motions,
    }
}

const SCENES: &[Scene] = &[
    scene("team", 106, 30, true),
    scene("work", 106, 50, true),
    scene("tabs", 106, 30, true),
    scene("mode", 106, 30, false),
    scene("toast", 106, 30, false),
    scene("collapsed", 106, 30, true),
    scene("mobile", 44, 30, false),
    scene("team-80", 80, 30, false),
    scene("team-120", 120, 30, false),
    scene("narrow-18", 80, 30, false),
    scene("roles-collapsed", 106, 40, false),
    scene("work-collapsed", 106, 40, false),
    scene("flat", 106, 30, false),
    scene("recent", 106, 40, false),
    scene("expired", 106, 40, false),
    scene("copy", 106, 30, false),
    scene("overlay-help", 106, 40, false),
    scene("overlay-navigator", 106, 40, false),
    scene("overlay-rename", 106, 30, false),
    scene("overlay-confirm", 106, 30, false),
    scene("overlay-settings", 106, 40, false),
    scene("overlay-global-menu", 106, 30, false),
    scene("overlay-context-menu", 106, 30, false),
    scene("anim-1", 106, 40, true),
    scene("anim-2", 106, 40, true),
    scene("anim-3", 106, 40, true),
];

/// Offsets from the animation origin: mid-bucket for the spinner (80 ms steps)
/// and spread over the 640 ms tab sweep and the 1.6 s blocked pulse.
const ANIM_OFFSETS_MS: [u64; 3] = [200, 360, 520];
const NOW_UNIX_MS: u64 = 10_000_000_000;

fn work_task() -> crate::protocol::OrbitShellActiveTask {
    crate::protocol::OrbitShellActiveTask {
        task_id: "task-classic".into(),
        worker_terminal_id: "term_pane_3".into(),
        role: Some("reviewer".into()),
        title: "Review 界 changes".into(),
        state: "blocked".into(),
        created_at_ms: 9_999_880_000,
        blocked: true,
        awaiting_approval: true,
        steps: Vec::new(),
        step_total: None,
        parallel: None,
        steps_omitted: None,
    }
}

fn recent_task(id: &str, outcome: &str, age_ms: u64) -> crate::protocol::OrbitShellRecentTask {
    crate::protocol::OrbitShellRecentTask {
        task_id: id.into(),
        outcome: outcome.into(),
        summary: format!("summary 界 {id}"),
        finished_at_ms: NOW_UNIX_MS - age_ms,
    }
}

fn config_for(theme: &str, motion: &str, scene: &str) -> Config {
    let mut config: Config = toml::from_str(&format!(
        "[theme]\nname = '{theme}'\n[ui]\nanimations = '{motion}'\n"
    ))
    .unwrap();
    match scene {
        "work" => {
            config.ui.sidebar_width = 48;
            config.ui.sidebar_max_width = 60;
        }
        "narrow-18" => config.ui.sidebar_width = 18,
        "flat" => config.ui.sidebar_team = false,
        _ => {}
    }
    config
}

fn snapshot_for(scene: &str) -> ClientShellSnapshot {
    let mut projected = team_sidebar::team_snapshot();
    let orbit = projected.orbit.as_mut().unwrap();
    match scene {
        "work" | "work-collapsed" | "roles-collapsed" | "anim-1" | "anim-2" | "anim-3" => {
            orbit.active_tasks.push(work_task());
        }
        "recent" | "expired" => {
            orbit.active_tasks.push(work_task());
            let age = if scene == "recent" { 300_000 } else { 900_000 };
            orbit.recent_tasks = vec![
                recent_task("task-done", "completed", age),
                recent_task("task-failed", "failed", age + 1_000),
                recent_task("task-cancel", "cancelled", age + 2_000),
            ];
        }
        _ => {}
    }
    if scene == "tabs" {
        projected.tabs.extend((2..=8).map(|number| ClientShellTab {
            tab_id: format!("tab_{number}"),
            workspace_id: "ws_1".into(),
            number,
            label: format!("test {number}"),
            custom_label: true,
            zoomed: false,
            focused: false,
            agent_status: AgentStatus::Idle,
        }));
    }
    projected
}

fn visible_notification(base: Instant) -> ClientVisibleNotification {
    let shown_at = base - Duration::from_secs(1);
    ClientVisibleNotification {
        endpoint_id: ClientEndpointId::Local,
        event: SemanticNotification {
            kind: SemanticNotificationKind::Custom,
            title: "Review ready".into(),
            body: Some("Check 界 output".into()),
            sound: None,
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: Some(crate::config::ToastAsterPosition::BottomRight),
        },
        shown_at,
        deadline: shown_at + Duration::from_secs(60),
    }
}

fn prepare(state: &mut ClientShellState, scene: &str, base: Instant) {
    match scene {
        "mode" => state.mode = ClientShellMode::Navigate,
        "toast" => state.visible_notification = Some(visible_notification(base)),
        "collapsed" => state.sidebar_collapsed = true,
        "roles-collapsed" => {
            state.collapsed_team_groups.insert("role:reviewer".into());
            state.collapsed_team_groups.insert("other".into());
        }
        "work-collapsed" => {
            state.collapsed_team_groups.insert("work".into());
        }
        "copy" => {
            assert!(state.show_copy_feedback(base));
        }
        "overlay-help" => {
            state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                query: TextEditor::default(),
                search_focused: false,
                scroll: 0,
            }));
        }
        "overlay-navigator" => state.open_navigator_overlay(),
        "overlay-rename" => state.open_rename_workspace_overlay(),
        "overlay-confirm" => state.open_confirm_close_overlay("ws_1".into()),
        "overlay-settings" => state.open_settings_overlay(),
        "overlay-global-menu" => {
            state.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
                highlighted: 0,
            }));
        }
        "overlay-context-menu" => state.open_workspace_context_menu("ws_1".into(), 6, 4),
        _ => {}
    }
}

/// One scene's frame as text plus a hash of every cell attribute.
fn frame_text(
    scene: &Scene,
    theme: &str,
    motion: &str,
    apply_style: &dyn Fn(&mut Config),
) -> String {
    let mut config = config_for(theme, motion, scene.name);
    apply_style(&mut config);
    let projected = snapshot_for(scene.name);
    let mut state = team_sidebar::team_state(config, projected.clone());
    let base = Instant::now();
    // A fixed origin: animation phases come from the offsets below, never from
    // how long the test happened to run.
    state.animations = super::super::animation::ClientAnimationClock::new(base);
    state.wall_clock = super::super::work_board::WallClock::at(base, NOW_UNIX_MS);
    state.orbit_clock = None;
    prepare(&mut state, scene.name, base);
    let mut at = base;
    if let Some(step) = scene.name.strip_prefix("anim-") {
        let step: usize = step.parse().unwrap();
        at = base + Duration::from_millis(ANIM_OFFSETS_MS[step - 1]);
        // A status change at the origin flashes and highlights its row; a tab
        // report starts the tab sweep.
        let endpoint = ClientEndpointId::Local;
        state
            .animations
            .observe_snapshot(&endpoint, Some(1), &projected, base);
        let mut changed = projected;
        changed.agents[1].agent_status = AgentStatus::Idle;
        changed.agents[1].state_change_seq += 1;
        state.set_snapshot(Box::new(changed.clone()));
        state
            .animations
            .observe_snapshot(&endpoint, Some(1), &changed, base);
        state
            .animations
            .record_tab_report(&endpoint, "tab_1", AgentStatus::Blocked, base);
    }
    let frame = state.compose_at(scene.cols, scene.rows, at).unwrap();
    let mut text = String::new();
    let mut hash = 0xcbf29ce484222325u64;
    for row in frame.cells.chunks(scene.cols as usize) {
        for cell in row {
            text.push_str(&cell.symbol);
            for byte in format!(
                "{}:{}:{}:{}:{:?};",
                cell.fg, cell.bg, cell.modifier, cell.skip, cell.hyperlink
            )
            .bytes()
            {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        text.push_str("|\n");
    }
    text.push_str(&format!("style={hash:016x}\n"));
    text
}

/// Every classic frame, as `(name, text)` in a fixed order.
fn render_matrix(apply_style: &dyn Fn(&mut Config)) -> Vec<(String, String)> {
    let mut frames = Vec::new();
    for theme in ["catppuccin", "catppuccin-latte", "terminal"] {
        for scene in SCENES {
            let motions: &[&str] = if scene.motions {
                &["full", "reduced", "off"]
            } else {
                &["full"]
            };
            for motion in motions {
                frames.push((
                    format!("{theme}/{motion}/{}", scene.name),
                    frame_text(scene, theme, motion, apply_style),
                ));
            }
        }
    }
    frames
}

fn fixture_text() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/classic-phase1.txt"
    ))
    .unwrap()
}

fn expected_frames() -> Vec<(String, String)> {
    fixture_text()
        .split("=== ")
        .filter(|block| !block.is_empty())
        .map(|block| {
            let (name, text) = block.split_once('\n').unwrap();
            (name.to_owned(), text.to_owned())
        })
        .collect()
}

// --- style-dependent entry points (replaced when regenerating the fixture
// from the pre-phase tree, which has no `ui.style`) ---

#[test]
fn classic_visual_characterization() {
    // Classic stays identical to the pre-foundation tree for either glyph
    // setting; modern redraws the sidebar from phase 2 on and has its own tests.
    let expected = expected_frames();
    for glyphs in [
        crate::config::UiGlyphsConfig::Unicode,
        crate::config::UiGlyphsConfig::Ascii,
    ] {
        let actual = render_matrix(&|config: &mut Config| {
            config.ui.style = crate::config::UiStyleConfig::Classic;
            config.ui.glyphs = glyphs;
        });
        assert_eq!(actual.len(), expected.len());
        for ((name, text), (expected_name, expected_text)) in actual.iter().zip(&expected) {
            assert_eq!(name, expected_name);
            assert_eq!(text, expected_text, "{name} {glyphs:?}");
        }
    }
}

#[test]
#[ignore = "regenerates the fixture; run from the pre-phase tree (see the header)"]
fn generate_classic_fixture() {
    let out = std::env::var("CLASSIC_FIXTURE_OUT").unwrap();
    let mut text = String::new();
    for (name, frame) in render_matrix(&|_| {}) {
        text.push_str(&format!("=== {name}\n{frame}"));
    }
    std::fs::write(out, text).unwrap();
}
