use super::modern_headers::bordered_surface;
use super::modern_sidebar::{lane_snapshot, modern_state};
use super::*;
use crate::ui::chrome::state::{StateCounts, StateKind};

#[test]
fn output_only_surfaces_do_not_rebuild_header_geometry() {
    let mut state = modern_state(26, lane_snapshot());
    let surface = bordered_surface(
        (104, 29),
        &[("pane_3", 0, 52), ("pane_2", 52, 52)],
        "pane_3",
    );
    state.set_pane_surface(surface.clone());
    let built = state.header_copy_builds;
    let storage = state.header_copies.as_ptr();
    for revision in 1..=12 {
        let mut next = surface.clone();
        next.surface_revision = revision;
        next.panes[0].content_revision += revision;
        next.frame.cells[105].symbol = format!("{revision}");
        state.set_pane_surface(next);
    }
    assert_eq!(state.header_copy_builds, built);
    assert_eq!(state.header_copies.as_ptr(), storage);
    let mut changed = surface;
    changed.surface_revision = 13;
    changed.panes[0].rect.width -= 1;
    state.set_pane_surface(changed);
    assert_eq!(state.header_copy_builds, built + 1);
}

#[test]
fn classic_never_builds_header_copy_spans_and_live_modern_switch_is_prepared() {
    let mut config = Config::default();
    config.ui.style = crate::config::UiStyleConfig::Classic;
    let mut state = super::team_sidebar::team_state(config, lane_snapshot());
    state.set_pane_surface(bordered_surface((104, 29), &[("pane_3", 0, 104)], "pane_3"));
    assert_eq!(state.header_copy_builds, 0);
    assert!(state.header_copies.is_empty());
    state.config.ui_style = crate::config::UiStyleConfig::Modern;
    state.config.refresh_chrome();
    state.compose(130, 30).unwrap();
    assert!(!state.header_copies.is_empty());
}

#[test]
fn narrow_collapsed_roles_keep_approval_and_blocked_before_low_priority_counts() {
    for width in [18, 22, 26, 36] {
        for glyphs in [
            crate::config::UiGlyphsConfig::Unicode,
            crate::config::UiGlyphsConfig::Ascii,
        ] {
            let mut state = modern_state(width, lane_snapshot());
            state.config.ui_glyphs = glyphs;
            state.config.refresh_chrome();
            let team = state.team_sidebar.as_mut().unwrap();
            team.groups[0].title = "a-long-role-name".into();
            let mut counts = StateCounts::default();
            for kind in [
                StateKind::Approval,
                StateKind::Blocked,
                StateKind::Working,
                StateKind::Queued,
                StateKind::Idle,
            ] {
                counts.add(kind);
            }
            team.modern.roles[0].counts = counts;
            state
                .collapsed_team_groups
                .insert(team.groups[0].key.clone());
            let frame = state.compose(120, 60).unwrap();
            let rect = state
                .hits
                .team_groups
                .iter()
                .find(|(_, group)| *group == 0)
                .unwrap()
                .0;
            let line = &frame.cells[usize::from(rect.y) * 120 + usize::from(rect.x)
                ..usize::from(rect.y) * 120 + usize::from(rect.right())];
            let text: String = line.iter().map(|cell| cell.symbol.as_str()).collect();
            assert!(
                text.contains(&format!("{}1", state.config.chrome.glyphs.approval)),
                "width {width}: {text}"
            );
            assert!(
                text.contains(&format!("{}1", state.config.chrome.glyphs.blocked)),
                "width {width}: {text}"
            );
        }
    }
}

/// Fixed geometry, optimized-build benchmark; no PTY/server or timed model builds.
#[test]
#[ignore = "release render profile"]
fn phase6_open_flow_profile() {
    use std::time::Instant;
    for cols in [80u16, 120, 200] {
        for steps in [4usize, 32] {
            let mut snapshot = super::modern_flow::flow_snapshot();
            let task = &mut snapshot.orbit.as_mut().unwrap().active_tasks[1];
            let template = task.steps[0].clone();
            task.steps = (0..steps)
                .map(|i| {
                    let mut step = template.clone();
                    step.index = i as u16;
                    step.member = format!("worker-{i}");
                    step.state = if i + 1 == steps {
                        "active"
                    } else {
                        "handed_off"
                    }
                    .into();
                    step.note = (i >= steps.saturating_sub(8))
                        .then(|| "checked the result; hand off to the next worker".into());
                    step.next = step.note.clone();
                    step
                })
                .collect();
            task.step_total = Some(steps as u16);
            task.parallel = None;
            let mut state = modern_state(26, snapshot);
            state.config.animations = crate::config::UiAnimationsConfig::Off;
            let pane_width = cols - 26;
            state.set_pane_surface(bordered_surface(
                (pane_width, 39),
                &[
                    ("pane_3", 0, pane_width / 2),
                    ("pane_2", pane_width / 2, pane_width - pane_width / 2),
                ],
                "pane_3",
            ));
            for opened in [false, true] {
                if opened {
                    state.open_orbit_flow(&mut ClientShellInput::default());
                    if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
                        flow.task = 1;
                    }
                }
                for _ in 0..5 {
                    std::hint::black_box(state.compose(cols, 40).unwrap());
                }
                let mut times = Vec::new();
                for _ in 0..40 {
                    let now = Instant::now();
                    std::hint::black_box(state.compose(cols, 40).unwrap());
                    times.push(now.elapsed().as_micros());
                }
                times.sort_unstable();
                println!(
                    "flow cols={cols} steps={steps} open={opened} median_us={} p95_us={}",
                    times[20], times[38]
                );
            }
        }
    }
}

#[test]
fn branch_approval_is_an_optional_fact_distinct_from_generic_blocking() {
    for known in [None, Some(false), Some(true)] {
        for omit_agent in [false, true] {
            let mut snapshot = super::modern_flow::flow_snapshot();
            let orbit = snapshot.orbit.as_mut().unwrap();
            let task = &mut orbit.active_tasks[1];
            task.steps[2].state = "active".into();
            let mut value = serde_json::to_value(&task.steps[1]).unwrap();
            if let Some(approval) = known {
                value["awaiting_approval"] = serde_json::json!(approval);
            }
            task.steps[1] = serde_json::from_value(value).unwrap();
            for agent in &mut snapshot.agents {
                if ["pane_5", "pane_6"].contains(&agent.pane_id.as_str()) {
                    agent.agent_status = AgentStatus::Blocked;
                }
            }
            if omit_agent {
                snapshot.agents.retain(|agent| agent.pane_id != "pane_5");
            }
            let mut state = modern_state(26, snapshot);
            state.open_orbit_flow(&mut ClientShellInput::default());
            if let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_mut() {
                flow.task = 1;
            }
            state.compose(120, 40).unwrap();
            let Some(ClientShellOverlay::OrbitFlow(flow)) = state.overlay.as_ref() else {
                panic!("flow")
            };
            let states = &flow.layout.as_ref().unwrap().states;
            let expected = if known == Some(true) {
                StateKind::Approval
            } else if omit_agent {
                StateKind::Working
            } else {
                StateKind::Blocked
            };
            assert_eq!(states[1], expected, "{known:?}/{omit_agent}");
            assert_eq!(
                states[2],
                StateKind::Blocked,
                "a sibling is not an approval"
            );
        }
    }
}

#[test]
fn step_approval_json_is_optional_and_retained_when_present() {
    let old = serde_json::json!({"index":0,"member":"worker","kind":"codex","state":"active"});
    let step: crate::protocol::OrbitShellStep = serde_json::from_value(old.clone()).unwrap();
    assert!(serde_json::to_value(&step)
        .unwrap()
        .get("awaiting_approval")
        .is_none());
    for fact in [false, true] {
        let mut new = old.clone();
        new["awaiting_approval"] = serde_json::json!(fact);
        let step: crate::protocol::OrbitShellStep = serde_json::from_value(new).unwrap();
        assert_eq!(
            serde_json::to_value(step).unwrap()["awaiting_approval"],
            fact
        );
    }
}

#[test]
fn fused_header_copy_matches_the_characterized_copy_then_paint_pipeline() {
    for mode in [
        crate::config::UiAnimationsConfig::Full,
        crate::config::UiAnimationsConfig::Reduced,
        crate::config::UiAnimationsConfig::Off,
    ] {
        let mut state = modern_state(26, lane_snapshot());
        state.config.animations = mode;
        let mut surface = bordered_surface(
            (104, 29),
            &[("pane_3", 0, 52), ("pane_2", 52, 52)],
            "pane_3",
        );
        surface
            .frame
            .hyperlinks
            .push("https://example.invalid/source".into());
        for (x, text) in [(2, "stale title"), (30, "┬"), (31, "keep me")] {
            surface.frame.cells[x].symbol = text.into();
            surface.frame.cells[x].skip = true;
            surface.frame.cells[x].hyperlink = Some(0);
        }
        surface.frame.cells[106].hyperlink = Some(100);
        state.set_pane_surface(surface.clone());
        state.compose(130, 30).unwrap();
        let area = Rect::new(7, 3, 104, 29);
        let buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let mut expected = FrameData::from_ratatui_buffer(&buffer, None);
        expected
            .hyperlinks
            .push("https://example.invalid/chrome".into());
        let mut actual = expected.clone();
        let mut hits = state.hits.panes.clone();
        for hit in &mut hits {
            let pane = surface
                .panes
                .iter()
                .find(|p| p.pane_id == hit.pane_id)
                .unwrap();
            hit.rect = Rect::new(
                area.x + pane.rect.x,
                area.y + pane.rect.y,
                pane.rect.width,
                pane.rect.height,
            );
            hit.inner_rect = Rect::new(
                area.x + pane.inner_rect.x,
                area.y + pane.inner_rect.y,
                pane.inner_rect.width,
                pane.inner_rect.height,
            );
        }
        let now = std::time::Instant::now();
        let ctx = super::super::team_sidebar::TeamRenderContext {
            config: &state.config,
            endpoint_id: &state.active_endpoint_id,
            animation: super::super::animation::AnimationFrame::new(&state.animations, mode, now),
            focused_pane_id: Some("pane_3"),
            now_unix_ms: 10_000_000_000,
            recent: &state.recent,
            now,
        };
        let team = state.team_sidebar.as_ref().unwrap();
        super::super::blit_pane_surface_with_headers(
            &mut expected,
            &surface.frame,
            area,
            &state.header_copies,
        );
        super::super::modern::draw::pane_header::paint(&mut expected, &hits, team, &ctx);
        super::super::blit_pane_surface_modern(
            &mut actual,
            &surface,
            area,
            &state.header_copies,
            team,
            &ctx,
        );
        assert_eq!(actual.cells, expected.cells, "{mode:?}");
        assert_eq!(actual.hyperlinks, expected.hyperlinks);
        assert_eq!(actual.cursor, expected.cursor);
    }
}

#[test]
fn flow_popup_application_is_allocation_free_and_clears_hidden_links() {
    use super::super::composition::{apply_flow_popup, frame_for_composition};
    let area = Rect::new(0, 0, 120, 40);
    let popup = super::super::render::flow_popup(area).unwrap();
    let buffer = Buffer::empty(area);
    let mut frame = frame_for_composition(&buffer, Some(popup), true);
    frame.hyperlinks.push("https://example.com".into());
    for cell in &mut frame.cells {
        cell.hyperlink = Some(0);
    }
    let mut scratch = Buffer::empty(popup);
    crate::ui::chrome::draw::put_segment_inline(
        &mut scratch,
        popup.x,
        popup.y,
        popup.right(),
        "界 ☀️ 👩‍👩‍👧‍👦",
        Style::default(),
    );
    // A toast/search layer before the flow must not discard popup capacity.
    let underlay = FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 8, 2)), None);
    let count = super::alloc_count::allocations(|| {
        super::super::composition::replace_composed_frame(&mut frame, &buffer, None, Some(popup));
        super::super::blit_pane_surface_reusing(
            &mut frame,
            &underlay,
            Rect::new(popup.x + 1, popup.y + 1, 8, 2),
        );
        apply_flow_popup(&mut frame, &scratch);
    });
    assert_eq!(count, 0);
    assert_eq!(frame.cells[0].hyperlink, Some(0));
    assert_ne!(
        frame.cells[0].modifier & crate::protocol::modifier_to_u16(Modifier::DIM),
        0
    );
    let index = usize::from(popup.y) * 120 + usize::from(popup.x);
    assert_eq!(frame.cells[index].hyperlink, None);
    assert_eq!(frame.cells[index].symbol, "界");
    assert_eq!(
        frame.cells[index].modifier & crate::protocol::modifier_to_u16(Modifier::DIM),
        0
    );
}

#[test]
fn phase6_composed_snapshot_matrix() {
    let mut actual = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for glyphs in ["unicode", "ascii"] {
                for cols in [44u16, 80, 120, 200] {
                    for collapsed in [false, true] {
                        let config: Config = toml::from_str(&format!("[theme]\nname='{theme}'\n[ui]\nanimations='{motion}'\nglyphs='{glyphs}'\n")).unwrap();
                        let mut state = super::team_sidebar::team_state(
                            config,
                            super::modern_flow::flow_snapshot(),
                        );
                        state.sidebar_collapsed = collapsed;
                        let base = std::time::Instant::now();
                        state.animations = super::super::animation::ClientAnimationClock::new(base);
                        state.config.animations = match motion {
                            "off" => crate::config::UiAnimationsConfig::Off,
                            "reduced" => crate::config::UiAnimationsConfig::Reduced,
                            _ => crate::config::UiAnimationsConfig::Full,
                        };
                        state.wall_clock =
                            super::super::work_board::WallClock::at(base, 10_000_000_000);
                        state.orbit_clock = None;
                        state.mode = ClientShellMode::Navigate;
                        // Geometry comes from the same shell layout as the server request.
                        let layout = state.layout(cols, 40);
                        let mut pane_surface = bordered_surface(
                            (layout.pane_surface.width, layout.pane_surface.height),
                            &[("pane_3", 0, layout.pane_surface.width)],
                            "pane_3",
                        );
                        if cols == 44 {
                            for pane in &mut pane_surface.panes {
                                pane.inner_rect = pane.rect;
                            }
                            for cell in &mut pane_surface.frame.cells {
                                cell.symbol = " ".into();
                            }
                        }
                        state.set_pane_surface(pane_surface);
                        let now = base + std::time::Duration::from_millis(400);
                        let frame = state.compose_at(cols, 40, now).unwrap();
                        actual.push_str(&format!(
                            "=== {theme}/{motion}/{glyphs}/{cols}/collapsed={collapsed}\n"
                        ));
                        super::modern_notifications::capture(
                            &mut actual,
                            &frame.to_ratatui_buffer().unwrap(),
                        );
                        let mut outcome = ClientShellInput::default();
                        state.open_orbit_flow(&mut outcome);
                        let frame = state.compose_at(cols, 40, now).unwrap();
                        actual.push_str("--- flow ---\n");
                        super::modern_notifications::capture(
                            &mut actual,
                            &frame.to_ratatui_buffer().unwrap(),
                        );
                    }
                }
            }
        }
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/modern-polish-phase6.txt"
    );
    if std::env::var_os("ASTER_CAPTURE_PHASE6").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}

#[test]
fn modern_mobile_header_uses_only_configured_ascii_chrome() {
    let mut state = modern_state(26, lane_snapshot());
    state.config.ui_glyphs = crate::config::UiGlyphsConfig::Ascii;
    state.config.refresh_chrome();
    let area = Rect::new(0, 0, 44, 3);
    let mut buffer = Buffer::empty(area);
    super::super::mobile::render_mobile_header(
        &mut buffer,
        area,
        state.snapshot.as_deref().unwrap(),
        &state.config,
        state.attention_summary,
        &mut ShellHitMap::default(),
    );
    let non_ascii: Vec<_> = buffer
        .content
        .iter()
        .filter(|cell| !cell.symbol().is_ascii())
        .map(|cell| cell.symbol())
        .collect();
    assert!(non_ascii.is_empty(), "{non_ascii:?}");
    state.mode = ClientShellMode::Navigate;
    let frame = state.compose(44, 40).unwrap();
    let non_ascii: Vec<_> = frame
        .cells
        .iter()
        .filter(|cell| !cell.symbol.is_ascii())
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(non_ascii.is_empty(), "switcher: {non_ascii:?}");
}

#[test]
fn ordinary_unicode_border_rows_reuse_the_initial_frame_allocation() {
    let state = modern_state(26, lane_snapshot());
    let team = state.team_sidebar.as_ref().unwrap();
    let now = std::time::Instant::now();
    let ctx = super::super::team_sidebar::TeamRenderContext {
        config: &state.config,
        endpoint_id: &state.active_endpoint_id,
        animation: super::super::animation::AnimationFrame::new(
            &state.animations,
            state.config.animations,
            now,
        ),
        focused_pane_id: None,
        now_unix_ms: 10_000_000_000,
        recent: &state.recent,
        now,
    };
    for count in [1usize, 15, 50] {
        let ids: Vec<_> = (0..count).map(|i| format!("pane_{i}")).collect();
        let width = 104 / count as u16;
        let panes: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    id.as_str(),
                    i as u16 * width,
                    if i + 1 == count {
                        104 - i as u16 * width
                    } else {
                        width
                    },
                )
            })
            .collect();
        let source = bordered_surface((104, 29), &panes, &ids[0]);
        let mut output = super::super::composition::frame_for_composition(
            &Buffer::empty(Rect::new(0, 0, 104, 29)),
            None,
            true,
        );
        let allocations = super::alloc_count::allocations(|| {
            super::super::blit_pane_surface_modern(
                &mut output,
                &source,
                Rect::new(0, 0, 104, 29),
                &[],
                team,
                &ctx,
            );
        });
        assert_eq!(allocations, 0, "{count} panes");
    }
}

#[test]
fn topology_digest_is_read_without_allocations_and_tracks_identity_not_output() {
    let mut state = modern_state(26, lane_snapshot());
    let mut source = bordered_surface(
        (104, 29),
        &[("pane_3", 0, 52), ("pane_2", 52, 52)],
        "pane_3",
    );
    source.splits.push(crate::protocol::PaneSurfaceSplit {
        direction: crate::protocol::PaneSurfaceSplitDirection::Horizontal,
        pos: 52,
        area: Rect::new(0, 0, 104, 29).into(),
        hit_rect: Rect::new(52, 0, 1, 29).into(),
        path: vec![false, true],
    });
    state.set_pane_surface(source.clone());
    let initial = state.current_pane_topology().unwrap();
    let allocations = super::alloc_count::allocations(|| {
        for _ in 0..12 {
            assert_eq!(state.current_pane_topology(), Some(initial));
        }
    });
    assert_eq!(allocations, 0);
    for revision in 1..=5 {
        source.surface_revision = revision;
        match revision {
            1 => {
                source.frame.cells[105].symbol = "x".into();
                source.panes[0].content_revision += 1;
                source.splits[0].pos -= 1;
            }
            2 => source.panes.reverse(),
            3 => source.splits[0].path.push(false),
            4 => source.splits[0].direction = crate::protocol::PaneSurfaceSplitDirection::Vertical,
            _ => source.panes[0].pane_id = "new-pane".into(),
        }
        state.set_pane_surface(source.clone());
        let digest = state.current_pane_topology().unwrap();
        assert_eq!(
            digest,
            super::super::pane_surface_topology_signature(&source)
        );
        if revision <= 2 {
            assert_eq!(digest, initial);
        } else {
            assert_ne!(digest, initial);
        }
    }
    state.reset_endpoint_projection();
    assert_eq!(state.current_pane_topology(), None);
}
