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

fn capture(out: &mut String, buffer: &Buffer) {
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
                        let area = Rect::new(0, 0, 44, height);
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
                            actual.push_str(&format!("=== card/{theme}/{glyphs}/{motion:?}/{elapsed}/{height}/{position:?}\n"));
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
                            "=== banner/{theme}/{glyphs}/{motion:?}/{elapsed}/{height}\n"
                        ));
                        capture(&mut actual, &buffer);
                    }
                }
            }
            for position in CLIPBOARD {
                let area = Rect::new(0, 0, 44, 3);
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
                actual.push_str(&format!("=== clipboard/{theme}/{glyphs}/{position:?}\n"));
                capture(&mut actual, &buffer);
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

fn copy_mode_fixture() -> ClientCopyModeState {
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
            for cols in [50, 64, 80, 120] {
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
    assert_eq!(covered, [Rect::new(14, 1, 8, 1), Rect::new(16, 2, 6, 1)]);
}
