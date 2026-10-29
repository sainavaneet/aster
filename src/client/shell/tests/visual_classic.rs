use super::*;

// Captured from d870c4a8, before the chrome helper extraction. Keep these
// fixtures unchanged when later modern surfaces start using the foundation.
#[test]
fn classic_visual_characterization() {
    for style in [
        crate::config::UiStyleConfig::Classic,
        crate::config::UiStyleConfig::Modern,
    ] {
        for glyphs in [
            crate::config::UiGlyphsConfig::Unicode,
            crate::config::UiGlyphsConfig::Ascii,
        ] {
            assert_classic_fixture(style, glyphs);
        }
    }
}

fn assert_classic_fixture(
    style: crate::config::UiStyleConfig,
    glyphs: crate::config::UiGlyphsConfig,
) {
    let mut actual = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for scene in [
                "team",
                "work",
                "tabs",
                "mode",
                "toast",
                "collapsed",
                "mobile",
            ] {
                let mut config: Config = toml::from_str(&format!(
                    "[theme]\nname = '{theme}'\n[ui]\nanimations = '{motion}'\n"
                ))
                .unwrap();
                config.ui.style = style;
                config.ui.glyphs = glyphs;
                let mut projected = team_sidebar::team_snapshot();
                if scene == "work" {
                    config.ui.sidebar_width = 48;
                    config.ui.sidebar_max_width = 60;
                    projected.orbit.as_mut().unwrap().active_tasks.push(
                        crate::protocol::OrbitShellActiveTask {
                            task_id: "task-classic".into(),
                            worker_terminal_id: "term_pane_3".into(),
                            role: Some("reviewer".into()),
                            title: "Review 界 changes".into(),
                            state: "blocked".into(),
                            created_at_ms: 9_999_880_000,
                            blocked: true,
                            awaiting_approval: true,
                        },
                    );
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
                let mut state = team_sidebar::team_state(config, projected);
                let future = std::time::Instant::now() + std::time::Duration::from_secs(3600);
                // Clamp elapsed animation time at zero; do not depend on test scheduling.
                state.animations = super::super::animation::ClientAnimationClock::new(future);
                state.wall_clock = super::super::work_board::WallClock::at(future, 10_000_000_000);
                state.orbit_clock = None;
                state.sidebar_collapsed = scene == "collapsed";
                if scene == "mode" {
                    state.mode = ClientShellMode::Navigate;
                }
                if scene == "toast" {
                    let now = std::time::Instant::now() - std::time::Duration::from_secs(1);
                    state.visible_notification = Some(ClientVisibleNotification {
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
                        shown_at: now,
                        deadline: now + std::time::Duration::from_secs(60),
                    });
                }
                let width = if scene == "mobile" { 44 } else { 106 };
                let frame = state
                    .compose(width, if scene == "work" { 50 } else { 30 })
                    .unwrap();
                actual.push_str(&format!("{theme}/{motion}/{scene}\n"));
                // Text remains reviewable; hash every style/skip/link cell attribute too.
                let mut hash = 0xcbf29ce484222325u64;
                for row in frame.cells.chunks(width as usize) {
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
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/classic-phase0.txt"
    );
    assert_eq!(actual, std::fs::read_to_string(fixture).unwrap());
}
