use super::modern_sidebar::{lane_snapshot, modern_state};
use super::*;
use crate::protocol::{PaneSurfacePane, SurfaceRect};
use ratatui::widgets::{Block, Borders, Widget};

const COLS: u16 = 130;
const ROWS: u16 = 30;

/// A pane surface of boxed panes, `(pane id, x, width)`, each full height, as
/// the server draws them with `ui.pane_borders` on.
pub(super) fn bordered_surface(
    area: (u16, u16),
    panes: &[(&str, u16, u16)],
    focused: &str,
) -> PaneSurfaceFrame {
    let (width, height) = area;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut entries = Vec::new();
    for (pane_id, x, pane_width) in panes {
        let rect = Rect::new(*x, 0, *pane_width, height);
        Block::default()
            .borders(Borders::ALL)
            .render(rect, &mut buffer);
        let surface_rect = |rect: Rect| SurfaceRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        entries.push(PaneSurfacePane {
            pane_id: (*pane_id).into(),
            content_revision: 0,
            rect: surface_rect(rect),
            inner_rect: surface_rect(Rect::new(x + 1, 1, pane_width - 2, height - 2)),
            scrollbar_rect: None,
            scroll: None,
            focused: *pane_id == focused,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: 1,
        surface_revision: 1,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
        panes: entries,
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

/// The pane surface starts to the right of the 26-column sidebar.
fn header_row(frame: &FrameData, state: &ClientShellState) -> String {
    let y = state.hits.panes[0].rect.y;
    super::frame_rows(frame)[usize::from(y)].clone()
}

fn state_with_panes(widths: &[(&str, u16, u16)], focused: &str) -> ClientShellState {
    let mut state = modern_state(26, lane_snapshot());
    state.set_pane_surface(bordered_surface((COLS - 26, ROWS - 1), widths, focused));
    state
}

fn two_panes(width: u16, state_focus: &str) -> ClientShellState {
    state_with_panes(
        &[("pane_3", 0, width), ("pane_2", width, width)],
        state_focus,
    )
}

fn header_of(width: u16) -> String {
    let mut state = two_panes(width, "pane_2");
    let frame = state.compose(COLS, ROWS).unwrap();
    header_row(&frame, &state)
}

#[test]
fn a_worker_header_shows_role_member_model_title_state_and_age() {
    let row = header_of(100);
    for needle in [
        "▌reviewer ⚑ rev-codex codex·gpt-5 · approve task title",
        "4m ┐",
    ] {
        assert!(row.contains(needle), "{needle:?} missing: {row}");
    }
    // The relayed worker shows its step line before its age.
    let mut relayed = state_with_panes(&[("pane_2", 0, 100)], "pane_2");
    let frame = relayed.compose(COLS, ROWS).unwrap();
    let row = header_row(&frame, &relayed);
    assert!(
        row.contains("rev-claude claude·opus · relay task title"),
        "{row}"
    );
    assert!(row.contains("step 2/3 12m"), "{row}");
}

#[test]
fn headers_drop_parts_in_priority_order_as_the_pane_narrows() {
    // Wide: everything. Then the model, the kind and title go before the age,
    // and the role name and member stay down to the narrowest panes.
    let at = |width| header_of(width);
    let wide = at(100);
    assert!(wide.contains("codex·gpt-5") && wide.contains("approve task title"));
    let medium = at(40);
    assert!(
        medium.contains("rev-codex · approve t…") && !medium.contains("codex·"),
        "{medium}"
    );
    assert!(medium.contains("4m"));
    let narrow = at(30);
    assert!(
        narrow.contains("reviewer ⚑ rev-codex") && narrow.contains("4m"),
        "{narrow}"
    );
    assert!(!narrow.contains("approve"), "{narrow}");
    let tiny = at(12);
    assert!(tiny.contains('⚑'), "state glyph survives: {tiny}");
    assert!(
        tiny.contains('┌') && tiny.contains('┐'),
        "corners intact: {tiny}"
    );
}

#[test]
fn headers_never_overwrite_junctions_and_skip_non_workers_and_tiny_panes() {
    let mut state = two_panes(60, "pane_3");
    // A shared divider drops a tee into the first pane's top border.
    let mut surface = bordered_surface(
        (COLS - 26, ROWS - 1),
        &[("pane_3", 0, 60), ("pane_2", 60, 60)],
        "pane_3",
    );
    let tee_x = 30usize;
    surface.frame.cells[tee_x].symbol = "┬".into();
    state.set_pane_surface(surface);
    let frame = state.compose(COLS, ROWS).unwrap();
    let y = usize::from(state.hits.panes[0].rect.y);
    let origin = usize::from(state.hits.panes[0].rect.x);
    let cell = |x: usize| {
        frame.cells[y * usize::from(frame.width) + origin + x]
            .symbol
            .as_str()
    };
    assert_eq!(cell(tee_x), "┬", "the divider tee is intact");
    assert_eq!(cell(0), "┌");
    assert!(super::frame_rows(&frame)[y].contains("▌reviewer"));

    // A pane that hosts no worker keeps its plain border; so does a 5-wide pane.
    let mut plain = state_with_panes(&[("pane_8", 0, 40), ("pane_3", 40, 5)], "pane_8");
    let frame = plain.compose(COLS, ROWS).unwrap();
    let row = header_row(&frame, &plain);
    assert!(!row.contains('▌'), "{row}");
}

#[test]
fn classic_leaves_the_border_row_alone_and_ascii_uses_only_ascii_inside_it() {
    let mut classic = two_panes(100, "pane_2");
    classic.config.ui_style = crate::config::UiStyleConfig::Classic;
    classic.config.refresh_chrome();
    let frame = classic.compose(COLS, ROWS).unwrap();
    assert!(!header_row(&frame, &classic).contains('▌'));

    let mut ascii = two_panes(100, "pane_2");
    ascii.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    ascii.config.refresh_chrome();
    let frame = ascii.compose(COLS, ROWS).unwrap();
    let row = header_row(&frame, &ascii);
    let start = row.find('┌').unwrap();
    for ch in row[start..].chars() {
        assert!(
            ch.is_ascii() || "┌┐─│└┘┬┴┼├┤".contains(ch),
            "non-ASCII {ch:?} in {row}"
        );
    }
    assert!(
        row.contains("|reviewer ? rev-codex codex.gpt-5 . approve task title"),
        "{row}"
    );
}

#[test]
fn recent_changes_tint_the_header_and_decay_by_motion_mode() {
    let background = |mode: crate::config::UiAnimationsConfig, offset_ms: u64| {
        let mut config = Config::default();
        config.ui.animations = mode;
        let mut state = team_sidebar::team_state(config, lane_snapshot());
        let base = std::time::Instant::now();
        state.animations = super::super::animation::ClientAnimationClock::new(base);
        state.wall_clock = super::super::work_board::WallClock::at(base, 10_000_000_000);
        state.orbit_clock = None;
        state.set_pane_surface(bordered_surface(
            (COLS - 26, ROWS - 1),
            &[("pane_3", 0, 100)],
            "pane_3",
        ));
        let endpoint = ClientEndpointId::Local;
        let mut before = lane_snapshot();
        state
            .animations
            .observe_snapshot(&endpoint, Some(1), &before, base);
        before
            .agents
            .iter_mut()
            .find(|a| a.pane_id == "pane_3")
            .unwrap()
            .agent_status = AgentStatus::Idle;
        before
            .agents
            .iter_mut()
            .find(|a| a.pane_id == "pane_3")
            .unwrap()
            .state_change_seq += 1;
        state
            .animations
            .observe_snapshot(&endpoint, Some(1), &before, base);
        let frame = state
            .compose_at(
                COLS,
                ROWS,
                base + std::time::Duration::from_millis(offset_ms),
            )
            .unwrap();
        let x = usize::from(state.hits.panes[0].rect.x) + 2;
        let y = usize::from(state.hits.panes[0].rect.y);
        frame.cells[y * usize::from(frame.width) + x].bg
    };
    use crate::config::UiAnimationsConfig::{Full, Off, Reduced};
    let fresh = background(Full, 50);
    let calm = background(Full, 2500);
    assert_ne!(fresh, calm, "a fresh change tints the header");
    assert_eq!(
        background(Reduced, 50),
        fresh,
        "reduced keeps the one-shot highlight"
    );
    assert_eq!(background(Reduced, 2500), calm, "and it decays");
    assert_eq!(background(Off, 50), calm, "off never tints");
}

#[test]
fn tabs_show_role_chips_and_counts_in_the_active_glyph_set() {
    let mut state = two_panes(60, "pane_2");
    let frame = state.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].clone();
    assert!(tabs.contains("▪▫ ▲1 ⚑1 ◐1"), "{tabs}");
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    let frame = state.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].clone();
    assert!(tabs.contains("#. !1 ?1 *1"), "{tabs}");
    state.config.ui_style = crate::config::UiStyleConfig::Classic;
    state.config.refresh_chrome();
    let frame = state.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].clone();
    assert!(!tabs.contains("*1") && !tabs.contains('▪'), "{tabs}");
}

#[test]
fn tab_activity_is_rebuilt_with_each_snapshot_and_never_scanned_per_frame() {
    let mut state = two_panes(60, "pane_2");
    state.compose(COLS, ROWS).unwrap();
    let activity = state.tab_activity["tab_1"];
    assert_eq!(
        (
            activity.working,
            activity.blocked + activity.approval,
            activity.approval > 0
        ),
        (1, 2, true)
    );
    // An approval resolving changes the snapshot, and the cache with it.
    let mut next = lane_snapshot();
    next.orbit.as_mut().unwrap().active_tasks.clear();
    state.set_snapshot(Box::new(next));
    let activity = state.tab_activity["tab_1"];
    assert_eq!((activity.blocked, activity.approval), (2, 0));
}

#[test]
fn headers_survive_zoom_mobile_and_a_collapsed_sidebar() {
    // A zoomed pane fills the surface: one wide header.
    let mut zoomed = state_with_panes(&[("pane_3", 0, COLS - 26)], "pane_3");
    let frame = zoomed.compose(COLS, ROWS).unwrap();
    assert!(header_row(&frame, &zoomed).contains("▌reviewer ⚑ rev-codex"));
    // A collapsed sidebar moves the pane surface left; the header follows it.
    let mut collapsed = state_with_panes(&[("pane_3", 0, 90)], "pane_3");
    collapsed.sidebar_collapsed = true;
    let frame = collapsed.compose(COLS, ROWS).unwrap();
    assert!(header_row(&frame, &collapsed).contains("▌reviewer ⚑ rev-codex"));
    // Mobile composes without a sidebar, and without panicking.
    let mut mobile = state_with_panes(&[("pane_3", 0, 40)], "pane_3");
    mobile.compose(44, 30).expect("mobile frame");
}

#[test]
fn header_planning_is_pure_and_prioritised() {
    use super::super::modern::draw::pane_header::plan;
    use super::super::modern::model::pane_header::HeaderWidths;
    let widths = HeaderWidths {
        role: 8,
        member: 9,
        kind: 5,
        model: 6,
        title: 18,
        step: 8,
    };
    assert_eq!(
        plan(1, &widths, 2).member,
        0,
        "nothing fits under the gutter and glyph"
    );
    let tight = plan(12, &widths, 2);
    assert!(tight.role > 0 && tight.member > 0 && tight.title == 0 && !tight.age);
    let medium = plan(40, &widths, 2);
    assert!(medium.title >= 8 && medium.age && !medium.kind);
    let wide = plan(100, &widths, 2);
    assert!(wide.kind && wide.model && wide.step && wide.age);
    assert_eq!(wide.title, 18, "the title takes at most its own width");
}

#[test]
fn header_writer_never_grows_incoming_cell_strings() {
    let mut state = state_with_panes(&[("pane_3", 0, 100)], "pane_3");
    let mut frame = state.compose(COLS, ROWS).unwrap().frame;
    let hit = &state.hits.panes[0];
    let start = usize::from(hit.rect.y) * usize::from(frame.width) + usize::from(hit.rect.x) + 1;
    let end = start + usize::from(hit.rect.width) - 2;
    for cell in &mut frame.cells[start..end] {
        cell.symbol = String::from("x");
    }
    let capacities: Vec<_> = frame.cells[start..end]
        .iter()
        .map(|cell| cell.symbol.capacity())
        .collect();
    let now = std::time::Instant::now();
    super::super::modern::draw::pane_header::paint(
        &mut frame,
        &state.hits.panes,
        state.team_sidebar.as_ref().unwrap(),
        &super::super::team_sidebar::TeamRenderContext {
            config: &state.config,
            endpoint_id: &ClientEndpointId::Local,
            animation: super::super::animation::AnimationFrame::new(
                &state.animations,
                state.config.animations,
                now,
            ),
            focused_pane_id: Some("pane_3"),
            now_unix_ms: 10_000_000_000,
            recent: &state.recent,
            now,
        },
    );
    let after: Vec<_> = frame.cells[start..end]
        .iter()
        .map(|cell| cell.symbol.capacity())
        .collect();
    assert_eq!(
        after, capacities,
        "painting cannot allocate even when incoming labels use one-byte cells"
    );
}

#[test]
fn header_preserves_grapheme_clusters_in_names_and_titles() {
    let mut projected = lane_snapshot();
    projected.orbit.as_mut().unwrap().roles[0].members[0].name = "cafe\u{301}".into();
    projected.orbit.as_mut().unwrap().active_tasks[0].title = "fix 👨‍👩‍👧‍👦 parser".into();
    let mut state = modern_state(26, projected);
    state.set_pane_surface(bordered_surface(
        (COLS - 26, ROWS - 1),
        &[("pane_3", 0, 100)],
        "pane_3",
    ));
    let frame = state.compose(COLS, ROWS).unwrap();
    let row = header_row(&frame, &state);
    assert!(
        row.contains("cafe\u{301}"),
        "combining accent was lost: {row}"
    );
    assert!(row.contains("👨‍👩‍👧‍👦"), "joined emoji was split: {row}");
}

#[test]
fn headers_preserve_vertical_edge_stops() {
    let mut state = state_with_panes(&[("pane_3", 0, 100)], "pane_3");
    let mut surface = bordered_surface((COLS - 26, ROWS - 1), &[("pane_3", 0, 100)], "pane_3");
    surface.frame.cells[12].symbol = "│".into();
    state.set_pane_surface(surface);
    let frame = state.compose(COLS, ROWS).unwrap();
    let hit = &state.hits.panes[0];
    let index = usize::from(hit.rect.y) * usize::from(frame.width) + usize::from(hit.rect.x) + 12;
    assert_eq!(frame.cells[index].symbol, "│");
}

#[test]
fn tab_aggregation_is_scoped_and_ignores_presentation_changes() {
    let mut projected = lane_snapshot();
    let other = "tab_other";
    for agent in &mut projected.agents {
        if agent.pane_id == "pane_3" {
            agent.tab_id = other.into();
        }
    }
    let mut state = modern_state(26, projected);
    let activity = state.tab_activity.clone();
    assert_eq!(activity[other].approval, 1);
    assert_eq!(activity["tab_1"].approval, 0);
    assert_eq!(activity[other].blocked, 0);
    let first = state.tab_activity["tab_1"];
    state.compose(80, ROWS).unwrap();
    state.compose(200, ROWS).unwrap();
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    state.compose(120, ROWS).unwrap();
    assert_eq!(
        state.tab_activity["tab_1"], first,
        "geometry and glyphs do not affect snapshot facts"
    );
}

fn headers_snapshot_matrix() -> String {
    let mut out = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for glyphs in ["unicode", "ascii"] {
                for scene in ["active-approval-relay-parallel", "blocked-recent"] {
                    for (layout, cols) in [
                        ("80", 80),
                        ("120", 120),
                        ("200", 200),
                        ("mobile", 44),
                        ("collapsed", 120),
                    ] {
                        let config: Config = toml::from_str(&format!("[theme]\nname = '{theme}'\n[ui]\nanimations = '{motion}'\nglyphs = '{glyphs}'\n")).unwrap();
                        let mut projected = lane_snapshot();
                        projected.focused_pane_id = Some("pane_2".into());
                        if scene == "blocked-recent" {
                            projected.orbit.as_mut().unwrap().active_tasks[0].awaiting_approval =
                                false;
                        }
                        let mut state = team_sidebar::team_state(config, projected.clone());
                        state.sidebar_collapsed = layout == "collapsed";
                        let base = std::time::Instant::now();
                        state.animations = super::super::animation::ClientAnimationClock::new(base);
                        state.wall_clock =
                            super::super::work_board::WallClock::at(base, 10_000_000_000);
                        state.orbit_clock = None;
                        if scene == "blocked-recent" {
                            let mut before = projected.clone();
                            let agent = before
                                .agents
                                .iter_mut()
                                .find(|a| a.pane_id == "pane_3")
                                .unwrap();
                            agent.agent_status = AgentStatus::Working;
                            state.animations.observe_snapshot(
                                &ClientEndpointId::Local,
                                Some(1),
                                &before,
                                base,
                            );
                            projected
                                .agents
                                .iter_mut()
                                .find(|a| a.pane_id == "pane_3")
                                .unwrap()
                                .state_change_seq += 1;
                            state.set_snapshot(Box::new(projected.clone()));
                        }
                        state.animations.observe_snapshot(
                            &ClientEndpointId::Local,
                            Some(1),
                            &projected,
                            base,
                        );
                        if layout == "mobile" {
                            state.set_pane_surface(surface());
                        } else {
                            let area = state.layout(cols, ROWS).pane_surface;
                            let third = area.width / 3;
                            state.set_pane_surface(bordered_surface(
                                (area.width, area.height),
                                &[
                                    ("pane_3", 0, third),
                                    ("pane_2", third, third),
                                    ("pane_4", third * 2, area.width - third * 2),
                                ],
                                "pane_2",
                            ));
                        }
                        let frame = state
                            .compose_at(cols, ROWS, base + std::time::Duration::from_millis(160))
                            .unwrap();
                        out.push_str(&format!("=== {theme}/{motion}/{glyphs}/{scene}/{layout}\n"));
                        let mut hash = 0xcbf29ce484222325u64;
                        for row in frame.cells.chunks(cols as usize) {
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
    }
    out
}

#[test]
fn modern_header_snapshots() {
    let actual = headers_snapshot_matrix();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/modern-headers-phase3.txt"
    );
    if std::env::var_os("ASTER_CAPTURE_MODERN_HEADERS").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}

#[test]
fn header_cache_ignores_clock_and_geometry_but_refreshes_changed_task_facts() {
    let mut state = state_with_panes(&[("pane_2", 0, 100)], "pane_2");
    state.compose(COLS, ROWS).unwrap();
    let cached = &state.team_sidebar.as_ref().unwrap().modern.headers["pane_2"] as *const _;
    state.compose(200, ROWS).unwrap();
    state
        .config
        .set_palette(Palette::from_name("catppuccin-latte").unwrap());
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    state.compose(COLS, ROWS).unwrap();
    assert_eq!(
        cached,
        &state.team_sidebar.as_ref().unwrap().modern.headers["pane_2"] as *const _
    );
    let mut next = state.snapshot.as_deref().unwrap().clone();
    next.orbit
        .as_mut()
        .unwrap()
        .active_tasks
        .iter_mut()
        .find(|task| task.worker_terminal_id == "term_pane_2")
        .unwrap()
        .title = "changed parser task".into();
    state.set_snapshot(Box::new(next));
    let header = &state.team_sidebar.as_ref().unwrap().modern.headers["pane_2"];
    assert_eq!(header.title.as_deref(), Some("changed parser task"));
    assert_eq!(header.widths.title, 19);
}

#[test]
fn the_tab_bar_ends_with_an_attention_chip_that_yields_to_the_tabs() {
    let mut state = two_panes(60, "pane_2");
    let frame = state.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].trim_end().to_owned();
    assert!(tabs.ends_with("▲1 ⚑1 ◐1"), "{tabs}");
    // ASCII swaps the glyphs, not the layout.
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    let frame = state.compose(COLS, ROWS).unwrap();
    assert!(super::frame_rows(&frame)[0]
        .trim_end()
        .ends_with("!1 ?1 *1"));
    // Classic never draws it, and a narrow tab bar keeps its tabs instead.
    state.config.ui_style = crate::config::UiStyleConfig::Classic;
    state.config.refresh_chrome();
    let frame = state.compose(COLS, ROWS).unwrap();
    assert!(!super::frame_rows(&frame)[0].contains("!1"));
    let mut narrow = two_panes(10, "pane_2");
    let frame = narrow.compose(40, ROWS).unwrap();
    assert!(
        !super::frame_rows(&frame)[0].contains("▲1"),
        "no room for the chip"
    );
    // Resolving the approvals changes the snapshot, and the chip with it.
    let mut quiet = two_panes(60, "pane_2");
    let mut next = lane_snapshot();
    next.orbit.as_mut().unwrap().active_tasks.clear();
    next.agents.clear();
    quiet.set_snapshot(Box::new(next));
    let frame = quiet.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].trim_end().to_owned();
    assert!(!tabs.contains('▲') && !tabs.contains('◐'), "{tabs}");
}

// ---- review round 1: stale border text, per-pane capacity, a static recent
// marker, and blocked/approval kept apart.

#[test]
fn a_header_clears_the_servers_stale_border_title() {
    let mut state = state_with_panes(&[("pane_3", 0, 100)], "pane_3");
    let mut surface = bordered_surface((COLS - 26, ROWS - 1), &[("pane_3", 0, 100)], "pane_3");
    for (offset, ch) in
        "STALE-OLD-TITLE-OF-THE-SERVER-THAT-IS-VERY-LONG-AND-RUNS-UNDER-THE-NEW-HEADER"
            .chars()
            .enumerate()
    {
        surface.frame.cells[1 + offset].symbol = ch.to_string();
    }
    state.set_pane_surface(surface);
    let frame = state.compose(COLS, ROWS).unwrap();
    let row = header_row(&frame, &state);
    assert!(
        !row.contains("STALE") && !row.contains("OLD-TITLE"),
        "{row}"
    );
    assert!(
        row.contains("approve task title") && row.contains("4m ┐"),
        "{row}"
    );
    // The span between the title and the age is plain border again.
    let gap = row.split("approve task title").nth(1).unwrap();
    assert!(gap.trim_start_matches('─').starts_with(" 4m"), "{gap}");
}

#[test]
fn a_hidden_workers_header_text_never_grows_visible_cells() {
    let capacities = |hidden_name: &str| {
        let mut projected = lane_snapshot();
        let orbit = projected.orbit.as_mut().unwrap();
        orbit.roles[0].members[2].name = hidden_name.into();
        orbit.roles[0].members[2].pane_id = Some("pane_hidden".into());
        let mut state = modern_state(26, projected);
        state.set_pane_surface(bordered_surface(
            (COLS - 26, ROWS - 1),
            &[("pane_3", 0, 60), ("pane_2", 60, 60)],
            "pane_3",
        ));
        let frame = state.compose(COLS, ROWS).unwrap().frame;
        let width = usize::from(frame.width);
        // Content rows of both panes: nothing about them is a header.
        (3..8)
            .flat_map(|row| (27..140.min(width)).map(move |col| row * width + col))
            .map(|index| frame.cells[index].symbol.capacity())
            .collect::<Vec<_>>()
    };
    let plain = capacities("plain");
    let emoji = capacities("👩‍👩‍👧‍👦👩‍👩‍👧‍👦");
    assert_eq!(
        plain, emoji,
        "an off-screen worker's name changed visible content cells"
    );
    assert!(
        plain.iter().all(|capacity| *capacity <= 8),
        "content cells reserve header room: {plain:?}"
    );
}

fn changed_snapshot_state(
    mode: crate::config::UiAnimationsConfig,
) -> (ClientShellState, std::time::Instant) {
    let mut config = Config::default();
    config.ui.animations = mode;
    let mut quiet = lane_snapshot();
    // Baseline: the relayed worker is working and nobody waits on an approval.
    quiet
        .orbit
        .as_mut()
        .unwrap()
        .active_tasks
        .retain(|task| !task.awaiting_approval);
    let mut state = team_sidebar::team_state(config, quiet.clone());
    let base = std::time::Instant::now();
    state.animations = super::super::animation::ClientAnimationClock::new(base);
    state.wall_clock = super::super::work_board::WallClock::at(base, 10_000_000_000);
    state.orbit_clock = None;
    // The next snapshot: pane_2 finishes, pane_3 starts waiting on an approval.
    let mut next = lane_snapshot();
    for agent in &mut next.agents {
        if agent.pane_id == "pane_2" {
            agent.agent_status = AgentStatus::Idle;
        }
    }
    next.orbit.as_mut().unwrap().roles[0].members[1].state = "idle".into();
    state.set_snapshot(Box::new(next));
    state.set_pane_surface(bordered_surface(
        (COLS - 26, ROWS - 1),
        &[("pane_3", 0, 34), ("pane_2", 34, 34), ("pane_4", 68, 34)],
        "pane_4",
    ));
    (state, base)
}

fn member_cell_modifier(
    frame: &FrameData,
    state: &ClientShellState,
    pane: &str,
    glyph_offset: usize,
) -> u16 {
    let hit = state
        .hits
        .panes
        .iter()
        .find(|hit| hit.pane_id == pane)
        .expect("pane hit");
    let row = super::frame_rows(frame);
    let text = &row[usize::from(hit.rect.y)];
    let member = if pane == "pane_2" {
        "rev-claude"
    } else {
        "rev-codex"
    };
    let at = text.find(member).unwrap_or_else(|| {
        panic!(
            "{member} missing: {text} headers {:?} panes {:?}",
            state.team_sidebar.as_ref().map(|t| t
                .modern
                .headers
                .keys()
                .cloned()
                .collect::<Vec<_>>()),
            state
                .hits
                .panes
                .iter()
                .map(|h| (h.pane_id.clone(), h.rect))
                .collect::<Vec<_>>()
        )
    });
    let column = text[..at].chars().count() + glyph_offset;
    frame.cells[usize::from(hit.rect.y) * usize::from(frame.width) + column].modifier
}

#[test]
fn a_just_finished_or_newly_blocked_pane_keeps_a_static_marker_in_every_motion_mode() {
    use crate::config::UiAnimationsConfig::{Full, Off, Reduced};
    let underlined = ratatui::style::Modifier::UNDERLINED.bits();
    for mode in [Full, Reduced, Off] {
        let (mut state, base) = changed_snapshot_state(mode);
        // Long after any one-shot animation has decayed.
        let late = base + std::time::Duration::from_secs(5);
        let frame = state.compose_at(COLS, ROWS, late).unwrap().frame;
        for pane in ["pane_2", "pane_3"] {
            let modifier = member_cell_modifier(&frame, &state, pane, 0);
            assert!(
                modifier & underlined != 0,
                "{mode:?}: {pane} lost its recent marker"
            );
        }
        // A pane that did not change carries none.
        let quiet = state
            .hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == "pane_4")
            .unwrap();
        let row = super::frame_rows(&frame);
        let at = row[usize::from(quiet.rect.y)].find("probe").unwrap();
        let column = row[usize::from(quiet.rect.y)][..at].chars().count();
        let cell = &frame.cells[usize::from(quiet.rect.y) * usize::from(frame.width) + column];
        assert!(
            cell.modifier & underlined == 0,
            "{mode:?}: an unchanged pane is marked"
        );
        // The marker expires with the existing coarse tick, without motion.
        let much_later = base + std::time::Duration::from_secs(600);
        assert!(
            state.tick_copy_feedback(much_later),
            "{mode:?}: expiry asks for one repaint"
        );
        let frame = state.compose_at(COLS, ROWS, much_later).unwrap().frame;
        let modifier = member_cell_modifier(&frame, &state, "pane_2", 0);
        assert!(
            modifier & underlined == 0,
            "{mode:?}: the marker never expires"
        );
    }
}

#[test]
fn tabs_keep_blocked_and_approval_counts_apart() {
    let mut state = two_panes(60, "pane_2");
    let frame = state.compose(COLS, ROWS).unwrap();
    let tabs = super::frame_rows(&frame)[0].clone();
    assert!(
        tabs.contains("▪▫ ▲1 ⚑1 ◐1"),
        "one blocked and one approval must not merge: {tabs}"
    );
}

#[test]
fn recent_marks_reset_with_endpoint_identity_and_classic_expiry_is_quiet() {
    let (mut state, base) = changed_snapshot_state(crate::config::UiAnimationsConfig::Off);
    assert!(state
        .recent
        .is_recent("pane_2", base + std::time::Duration::from_secs(1)));
    state.config.ui_style = crate::config::UiStyleConfig::Classic;
    state.config.refresh_chrome();
    assert!(
        !state.tick_copy_feedback(base + std::time::Duration::from_secs(120)),
        "classic must not repaint for invisible marks"
    );
    let (mut state, base) = changed_snapshot_state(crate::config::UiAnimationsConfig::Off);
    let next = state.snapshot.as_deref().unwrap().clone();
    state.reset_endpoint_projection();
    assert!(state.pane_states.is_none());
    assert!(!state
        .recent
        .is_recent("pane_2", base + std::time::Duration::from_secs(1)));
    state.set_snapshot(Box::new(next));
    assert!(
        !state.recent.is_recent("pane_2", std::time::Instant::now()),
        "a new endpoint starts from a baseline"
    );
}

#[test]
fn long_graphemes_reserve_only_their_own_header_and_spans_rebuild_on_changes() {
    let cluster = format!("e{}", "\u{301}".repeat(140));
    assert!(cluster.len() > 255);
    let mut projected = lane_snapshot();
    projected.orbit.as_mut().unwrap().roles[0].members[0].name = cluster.clone();
    let mut state = modern_state(26, projected);
    state.set_pane_surface(bordered_surface(
        (COLS - 26, ROWS - 1),
        &[("pane_3", 0, 100)],
        "pane_3",
    ));
    let count = state.header_copies.len();
    let storage = state.header_copies.as_ptr();
    let frame = state.compose(COLS, ROWS).unwrap();
    assert!(header_row(&frame, &state).contains(&cluster));
    state.compose(200, ROWS).unwrap();
    assert_eq!(state.header_copies.len(), count);
    assert_eq!(
        state.header_copies.as_ptr(),
        storage,
        "frames reuse reservation spans"
    );
    let mut next = state.snapshot.as_deref().unwrap().clone();
    next.orbit.as_mut().unwrap().roles[0].members[0].name = "short".into();
    state.set_snapshot(Box::new(next));
    assert!(state
        .header_copies
        .iter()
        .all(|span| span.bytes < cluster.len()));
    state.set_pane_surface(bordered_surface(
        (COLS - 26, ROWS - 1),
        &[("pane_2", 0, 40)],
        "pane_2",
    ));
    assert!(state.header_copies.iter().all(|span| span.to <= 40));
}

#[test]
fn cleared_header_cells_cannot_keep_old_links_or_skip_flags() {
    let mut state = state_with_panes(&[("pane_3", 0, 100)], "pane_3");
    let mut surface = bordered_surface((COLS - 26, ROWS - 1), &[("pane_3", 0, 100)], "pane_3");
    surface
        .frame
        .hyperlinks
        .push("https://example.invalid/stale".into());
    for cell in &mut surface.frame.cells[1..99] {
        cell.hyperlink = Some(0);
        cell.skip = true;
    }
    state.set_pane_surface(surface);
    let frame = state.compose(COLS, ROWS).unwrap();
    let hit = &state.hits.panes[0];
    let start = usize::from(hit.rect.y) * usize::from(frame.width) + usize::from(hit.rect.x) + 1;
    assert!(frame.cells[start..start + 98]
        .iter()
        .all(|cell| cell.hyperlink.is_none() && !cell.skip));
}
