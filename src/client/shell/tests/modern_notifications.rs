use super::super::animation::{notification_transition_phase, NotificationTransitionPhase};
use super::super::modern::draw::toast;
use super::*;
use crate::app::state::CopyFeedback;
use crate::config::{ToastAsterPosition, ToastClipboardPosition, UiAnimationsConfig};
use crate::ui::chrome::state::StateKind;

const POSITIONS: [ToastAsterPosition; 4] = [
    ToastAsterPosition::TopLeft,
    ToastAsterPosition::TopRight,
    ToastAsterPosition::BottomLeft,
    ToastAsterPosition::BottomRight,
];
const CLIPBOARD: [ToastClipboardPosition; 6] = [
    ToastClipboardPosition::TopLeft,
    ToastClipboardPosition::TopCenter,
    ToastClipboardPosition::TopRight,
    ToastClipboardPosition::BottomLeft,
    ToastClipboardPosition::BottomCenter,
    ToastClipboardPosition::BottomRight,
];

fn config(theme: &str, glyphs: &str) -> ClientShellConfig {
    let config: Config = toml::from_str(&format!(
        "[theme]\nname='{theme}'\n[ui]\nglyphs='{glyphs}'\n"
    ))
    .unwrap();
    ClientShellConfig::from_config(&config)
}

pub(super) fn capture(out: &mut String, buffer: &Buffer) {
    let mut hash = 0xcbf29ce484222325u64;
    for row in buffer.content.chunks(usize::from(buffer.area.width)) {
        for cell in row {
            out.push_str(cell.symbol());
            for byte in format!(
                "{:?}:{:?}:{:?}:{};",
                cell.fg, cell.bg, cell.modifier, cell.skip
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

#[test]
fn short_toasts_and_offset_feedback_stay_inside_the_area() {
    let config = config("catppuccin", "unicode");
    let feedback = CopyFeedback {
        message: "日本語 👩‍👩‍👧‍👦 copied".into(),
    };
    for height in 1..=3 {
        for width in [1, 2, 3, 4, 12, 80] {
            let area = Rect::new(2, 2, width, height);
            for offset in [0, 1, 3, u16::MAX] {
                for position in POSITIONS {
                    let mut buffer = Buffer::empty(Rect::new(0, 0, width + 4, height + 4));
                    let rect = toast::draw_card(
                        &mut buffer,
                        area,
                        StateKind::Approval,
                        "日本語",
                        "body",
                        position,
                        offset,
                        &config,
                        NotificationTransitionPhase::SETTLED,
                    );
                    assert_eq!(rect.intersection(area), rect);
                }
                for position in CLIPBOARD {
                    let mut buffer = Buffer::empty(Rect::new(0, 0, width + 4, height + 4));
                    let rect = toast::draw_feedback(
                        &mut buffer,
                        area,
                        &feedback,
                        offset,
                        position,
                        &config,
                    );
                    assert_eq!(
                        rect.intersection(area),
                        rect,
                        "{area:?} {position:?} offset={offset}"
                    );
                }
            }
        }
    }
}

#[test]
fn toast_banner_feedback_and_diagnostic_draw_without_allocating() {
    let config = config("catppuccin-latte", "unicode");
    let area = Rect::new(0, 0, 80, 5);
    let mut buffer = Buffer::empty(area);
    let text = "日本語 👩‍👩‍👧‍👦";
    let feedback = CopyFeedback {
        message: text.into(),
    };
    let count = super::alloc_count::allocations(|| {
        toast::draw_card(
            &mut buffer,
            area,
            StateKind::Blocked,
            text,
            text,
            ToastAsterPosition::TopRight,
            0,
            &config,
            NotificationTransitionPhase::SETTLED,
        );
        toast::draw_banner(
            &mut buffer,
            area,
            StateKind::Done,
            text,
            Some(text),
            false,
            &config,
            NotificationTransitionPhase::SETTLED,
        );
        toast::draw_feedback(
            &mut buffer,
            area,
            &feedback,
            0,
            ToastClipboardPosition::BottomLeft,
            &config,
        );
        toast::draw_config_diagnostic(&mut buffer, area, text, &config, |_| {});
    });
    assert_eq!(count, 0);
}

#[test]
fn ascii_banner_uses_only_ascii_chrome() {
    let config = config("catppuccin", "ascii");
    let area = Rect::new(0, 0, 80, 1);
    let mut buffer = Buffer::empty(area);
    toast::draw_banner(
        &mut buffer,
        area,
        StateKind::Done,
        "done",
        Some("body"),
        false,
        &config,
        NotificationTransitionPhase::SETTLED,
    );
    assert!(buffer.content.iter().all(|cell| cell.symbol().is_ascii()));
}

#[test]
fn notifications_snapshot_matrix() {
    let mut actual = String::new();
    let base = std::time::Instant::now();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for glyphs in ["unicode", "ascii"] {
            let config = config(theme, glyphs);
            for cols in [44, 80, 120, 200] {
                for motion in [
                    UiAnimationsConfig::Full,
                    UiAnimationsConfig::Reduced,
                    UiAnimationsConfig::Off,
                ] {
                    for elapsed in [0, 300, 950] {
                        let phase = notification_transition_phase(
                            base,
                            base + std::time::Duration::from_secs(1),
                            base + std::time::Duration::from_millis(elapsed),
                            motion,
                        );
                        for height in 1..=3 {
                            let area = Rect::new(0, 0, cols, height);
                            for position in POSITIONS {
                                let mut buffer = Buffer::empty(area);
                                toast::draw_card(
                                    &mut buffer,
                                    area,
                                    StateKind::Approval,
                                    "日本語 👩‍👩‍👧‍👦 title",
                                    "a long body that must yield before the state glyph",
                                    position,
                                    0,
                                    &config,
                                    phase,
                                );
                                actual.push_str(&format!("=== card/{cols}/{theme}/{glyphs}/{motion:?}/{elapsed}/{height}/{position:?}\n"));
                                capture(&mut actual, &buffer);
                            }
                            let mut buffer = Buffer::empty(area);
                            toast::draw_banner(
                                &mut buffer,
                                area,
                                StateKind::Failed,
                                "endpoint rejected",
                                Some("日本語 body"),
                                false,
                                &config,
                                phase,
                            );
                            actual.push_str(&format!(
                            "=== banner/{cols}/{theme}/{glyphs}/{motion:?}/{elapsed}/{height}\n"
                        ));
                            capture(&mut actual, &buffer);
                        }
                    }
                }
                for position in CLIPBOARD {
                    let area = Rect::new(0, 0, cols, 3);
                    let mut buffer = Buffer::empty(area);
                    toast::draw_feedback(
                        &mut buffer,
                        area,
                        &CopyFeedback {
                            message: "日本語 copied".into(),
                        },
                        0,
                        position,
                        &config,
                    );
                    actual.push_str(&format!(
                        "=== clipboard/{cols}/{theme}/{glyphs}/{position:?}\n"
                    ));
                    capture(&mut actual, &buffer);
                }
            }
        }
    }
    golden("modern-notifications-phase5.txt", &actual);
}

fn golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/client/shell/tests/fixtures")
        .join(name);
    if std::env::var_os("ASTER_CAPTURE_PHASE5").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}

pub(super) fn copy_mode_fixture() -> ClientCopyModeState {
    ClientCopyModeState {
        pane_id: "pane_1".into(),
        content_revision: 0,
        geometry: (80, 24),
        alternate_screen_active: false,
        cursor: crate::api::schema::PaneTextPoint { row: 0, col: 0 },
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        entry_offset_from_bottom: 0,
        selection: None,
        search_prompt: None,
        search_query: String::new(),
        search_direction: None,
        search_matches: Vec::new(),
        search_total: 0,
        search_current: None,
        search_current_global: None,
        search_generation: 0,
        copy_after_search: false,
    }
}

#[test]
fn modern_mode_bar_snapshot_matrix() {
    let copy_mode = copy_mode_fixture();
    let mut actual = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for glyphs in ["unicode", "ascii"] {
            let config = config(theme, glyphs);
            let summary = super::super::modern::model::tabs::AttentionSummary {
                blocked: 1,
                approval: 2,
                working: 3,
            };
            for cols in [44, 50, 64, 80, 120, 200] {
                for mode in [
                    ClientShellMode::Terminal,
                    ClientShellMode::Prefix,
                    ClientShellMode::Navigate,
                    ClientShellMode::Resize,
                    ClientShellMode::Copy,
                ] {
                    for update in [false, true] {
                        let area = Rect::new(0, 0, cols, 1);
                        let mut buffer = Buffer::empty(area);
                        let bar = super::super::render::render_mode_bar(
                            &mut buffer,
                            area,
                            mode,
                            Some(&copy_mode),
                            None,
                            update,
                            &config.keybinds,
                            &config.palette,
                            Some(super::super::render::ModernModeBar {
                                config: &config,
                                summary,
                            }),
                        );
                        assert_eq!(bar.is_some(), mode != ClientShellMode::Terminal);
                        actual
                            .push_str(&format!("=== {theme}/{glyphs}/{cols}/{mode:?}/{update}\n"));
                        capture(&mut actual, &buffer);
                    }
                }
            }
        }
    }
    golden("modern-mode-bar-phase5.txt", &actual);
}

#[test]
fn copy_search_leaves_the_attention_chip_out_of_the_prompt() {
    let config = config("catppuccin", "unicode");
    let mut copy = copy_mode_fixture();
    copy.search_prompt = Some(ClientCopySearchPrompt {
        direction: crate::api::schema::PaneCopySearchDirection::Forward,
        query: TextEditor::default(),
    });
    let area = Rect::new(0, 0, 120, 1);
    let mut buffer = Buffer::empty(area);
    super::super::render::render_mode_bar(
        &mut buffer,
        area,
        ClientShellMode::Copy,
        Some(&copy),
        None,
        false,
        &config.keybinds,
        &config.palette,
        Some(super::super::render::ModernModeBar {
            config: &config,
            summary: super::super::modern::model::tabs::AttentionSummary {
                blocked: 1,
                approval: 2,
                working: 3,
            },
        }),
    );
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("COPY") && text.contains("enter search"));
    assert!(!text.contains('▲') && !text.contains('⚑'));
}

#[test]
fn diagnostic_width_and_occlusion_use_display_cells() {
    let config = config("catppuccin-latte", "unicode");
    let area = Rect::new(2, 1, 20, 2);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 4));
    let mut covered = Vec::new();
    assert_eq!(
        toast::draw_config_diagnostic(
            &mut buffer,
            area,
            "日本語\n\nnext\nclipped",
            &config,
            |rect| covered.push(rect)
        ),
        2
    );
    assert_eq!(covered, [Rect::new(11, 1, 11, 1), Rect::new(13, 2, 9, 1)]);
}

fn modern_bar(config: &ClientShellConfig) -> super::super::render::ModernModeBar<'_> {
    super::super::render::ModernModeBar {
        config,
        summary: super::super::modern::model::tabs::AttentionSummary {
            blocked: 1,
            approval: 2,
            working: 3,
        },
    }
}

#[test]
fn the_modern_mode_bar_draws_without_allocating_in_every_mode() {
    let mut searching = copy_mode_fixture();
    searching.search_prompt = Some(ClientCopySearchPrompt {
        direction: crate::api::schema::PaneCopySearchDirection::Backward,
        query: TextEditor::from("needle 👩‍👩‍👧‍👦"),
    });
    let mut matching = copy_mode_fixture();
    matching.search_query = "needle".into();
    matching.search_current_global = Some(2);
    matching.search_total = 14;
    let plain = copy_mode_fixture();
    for (theme, glyphs) in [("catppuccin", "unicode"), ("catppuccin-latte", "ascii")] {
        let config = config(theme, glyphs);
        for cols in [20, 50, 59, 60, 80, 200] {
            for mode in [
                ClientShellMode::Prefix,
                ClientShellMode::Navigate,
                ClientShellMode::Resize,
                ClientShellMode::Copy,
            ] {
                for copy in [&plain, &matching, &searching] {
                    for (error, update) in [(None, false), (None, true), (Some("lost link"), false)]
                    {
                        let area = Rect::new(0, 0, cols, 1);
                        let mut buffer = Buffer::empty(area);
                        let count = super::alloc_count::allocations(|| {
                            super::super::render::render_mode_bar(
                                &mut buffer,
                                area,
                                mode,
                                Some(copy),
                                error,
                                update,
                                &config.keybinds,
                                &config.palette,
                                Some(modern_bar(&config)),
                            );
                        });
                        assert_eq!(count, 0, "{theme}/{glyphs}/{cols}/{mode:?}/{error:?}");
                    }
                }
            }
        }
    }
}

fn is_ascii(buffer: &Buffer) -> bool {
    buffer.content.iter().all(|cell| cell.symbol().is_ascii())
}

#[test]
fn ascii_glyph_mode_emits_ascii_chrome_for_every_phase_5_surface() {
    let config = config("catppuccin", "ascii");
    let copy = copy_mode_fixture();
    for cols in [20, 50, 80, 120] {
        for mode in [
            ClientShellMode::Prefix,
            ClientShellMode::Navigate,
            ClientShellMode::Resize,
            ClientShellMode::Copy,
        ] {
            for update in [false, true] {
                let area = Rect::new(0, 0, cols, 1);
                let mut buffer = Buffer::empty(area);
                super::super::render::render_mode_bar(
                    &mut buffer,
                    area,
                    mode,
                    Some(&copy),
                    None,
                    update,
                    &config.keybinds,
                    &config.palette,
                    Some(modern_bar(&config)),
                );
                assert!(is_ascii(&buffer), "{cols}/{mode:?}: {:?}", buffer.content);
            }
        }
    }
    let area = Rect::new(0, 0, 80, 4);
    let feedback = CopyFeedback {
        message: "copied".into(),
    };
    for state in [
        StateKind::Approval,
        StateKind::Blocked,
        StateKind::Failed,
        StateKind::Done,
        StateKind::Idle,
    ] {
        let mut buffer = Buffer::empty(area);
        toast::draw_card(
            &mut buffer,
            area,
            state,
            "a title that is quite long to force the cut ellipsis on narrow areas",
            "body",
            ToastAsterPosition::TopLeft,
            0,
            &config,
            NotificationTransitionPhase::SETTLED,
        );
        toast::draw_banner(
            &mut buffer,
            area,
            state,
            "title",
            Some("body that needs to be cut by the narrow banner area"),
            false,
            &config,
            NotificationTransitionPhase::SETTLED,
        );
        toast::draw_feedback(
            &mut buffer,
            area,
            &feedback,
            0,
            ToastClipboardPosition::BottomCenter,
            &config,
        );
        toast::draw_config_diagnostic(&mut buffer, area, "invalid setting", &config, |_| {});
        assert!(is_ascii(&buffer), "{state:?}");
        let mut narrow = Buffer::empty(Rect::new(0, 0, 12, 4));
        toast::draw_card(
            &mut narrow,
            Rect::new(0, 0, 12, 4),
            state,
            "a title that is quite long",
            "body that is long too",
            ToastAsterPosition::TopLeft,
            0,
            &config,
            NotificationTransitionPhase::SETTLED,
        );
        assert!(is_ascii(&narrow), "narrow {state:?}");
    }
}

fn contrast(a: ratatui::style::Color, b: ratatui::style::Color) -> Option<f32> {
    let (ratatui::style::Color::Rgb(ar, ag, ab), ratatui::style::Color::Rgb(br, bg, bb)) = (a, b)
    else {
        return None;
    };
    Some(crate::ui::chrome::roles::contrast_ratio(
        (ar, ag, ab),
        (br, bg, bb),
    ))
}

#[test]
fn every_toast_banner_and_diagnostic_role_is_legible_in_every_theme() {
    let area = Rect::new(0, 0, 60, 3);
    for theme in crate::config::THEME_NAMES {
        let config = config(theme, "unicode");
        let mut buffers = Vec::new();
        for state in [
            StateKind::Approval,
            StateKind::Blocked,
            StateKind::Failed,
            StateKind::Done,
            StateKind::Idle,
            StateKind::Working,
        ] {
            let mut card = Buffer::empty(area);
            toast::draw_card(
                &mut card,
                area,
                state,
                "title",
                "body",
                ToastAsterPosition::TopLeft,
                0,
                &config,
                NotificationTransitionPhase::SETTLED,
            );
            let mut banner = Buffer::empty(area);
            toast::draw_banner(
                &mut banner,
                area,
                state,
                "title",
                Some("body"),
                false,
                &config,
                NotificationTransitionPhase::SETTLED,
            );
            buffers.extend([card, banner]);
        }
        let mut feedback = Buffer::empty(area);
        toast::draw_feedback(
            &mut feedback,
            area,
            &CopyFeedback {
                message: "copied".into(),
            },
            0,
            ToastClipboardPosition::TopLeft,
            &config,
        );
        let mut diagnostic = Buffer::empty(area);
        toast::draw_config_diagnostic(&mut diagnostic, area, "invalid setting", &config, |_| {});
        buffers.extend([feedback, diagnostic]);
        let copy = copy_mode_fixture();
        for mode in [
            ClientShellMode::Prefix,
            ClientShellMode::Navigate,
            ClientShellMode::Resize,
            ClientShellMode::Copy,
        ] {
            let area = Rect::new(0, 0, 200, 1);
            let mut bar = Buffer::empty(area);
            super::super::modern::draw::status::draw_mode_bar(
                &mut bar,
                area,
                super::super::modern::draw::status::ModeBarInput {
                    mode,
                    copy_mode: Some(&copy),
                    endpoint_error: None,
                    update_available: true,
                },
                &config,
                modern_bar(&config).summary,
            );
            buffers.push(bar);
        }
        for buffer in &buffers {
            for cell in buffer.content.iter().filter(|cell| {
                cell.symbol()
                    .trim()
                    .chars()
                    .next()
                    .is_some_and(|c| c != '▌')
            }) {
                let Some(ratio) = contrast(cell.fg, cell.bg) else {
                    continue;
                };
                assert!(
                    ratio >= crate::ui::chrome::roles::MIN_STATE_CONTRAST,
                    "{theme}: {:?} {:?} on {:?} = {ratio}",
                    cell.symbol(),
                    cell.fg,
                    cell.bg
                );
            }
        }
    }
}

fn toast_state(
    cols: u16,
    rows: u16,
    mode: ClientShellMode,
    position: ToastAsterPosition,
) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.mode = mode;
    let now = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or_else(std::time::Instant::now);
    state.visible_notification = Some(ClientVisibleNotification {
        endpoint_id: ClientEndpointId::Local,
        event: SemanticNotification {
            kind: SemanticNotificationKind::Finished,
            title: "Finished the assignment".into(),
            body: Some("checked every result and tests passed".into()),
            sound: None,
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: Some(position),
        },
        shown_at: now,
        deadline: now + std::time::Duration::from_secs(5),
    });
    state.copy_feedback = Some(CopyFeedback {
        message: "copied".into(),
    });
    let _ = (cols, rows);
    state
}

#[test]
fn bottom_toasts_banners_and_feedback_stay_above_the_mode_bar() {
    for (cols, rows, mode) in [
        (80, 24, ClientShellMode::Navigate),
        (120, 36, ClientShellMode::Navigate),
        (200, 50, ClientShellMode::Navigate),
        (80, 24, ClientShellMode::Prefix),
        (44, 24, ClientShellMode::Prefix),
        (30, 12, ClientShellMode::Prefix),
    ] {
        for position in [
            ToastAsterPosition::BottomLeft,
            ToastAsterPosition::BottomRight,
        ] {
            let mut state = toast_state(cols, rows, mode, position);
            state.config.clipboard_toast_position = ToastClipboardPosition::BottomCenter;
            let frame = state.compose(cols, rows).unwrap();
            let bar_row = frame_rows(&frame)[usize::from(rows) - 1].clone();
            assert!(
                bar_row.contains("esc") || bar_row.contains("PFX") || bar_row.contains("PREFIX"),
                "{cols}x{rows} {mode:?}: no mode bar on the last row: {bar_row}"
            );
            let toast = state.hits.notification_toast;
            assert!(!toast.is_empty(), "{cols}x{rows} {mode:?}");
            assert!(
                toast.bottom() < rows,
                "{cols}x{rows} {mode:?} {position:?}: toast {toast:?} covers the mode bar"
            );
        }
    }
}

#[test]
fn bottom_toasts_stay_above_a_bottom_tab_bar() {
    let mut state = toast_state(
        80,
        24,
        ClientShellMode::Terminal,
        ToastAsterPosition::BottomRight,
    );
    state.config.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let layout = state.layout(80, 24);
    assert!(!layout.tab_bar.is_empty() && layout.tab_bar.y > 12);
    state.compose(80, 24).unwrap();
    assert!(state.hits.notification_toast.bottom() <= layout.tab_bar.y);
}

#[test]
fn classic_toasts_keep_the_whole_screen() {
    let mut state = toast_state(
        80,
        24,
        ClientShellMode::Navigate,
        ToastAsterPosition::BottomRight,
    );
    state.config.ui_style = crate::config::UiStyleConfig::Classic;
    state.config.refresh_chrome();
    state.compose(80, 24).unwrap();
    assert_eq!(state.hits.notification_toast.bottom(), 24);
}
