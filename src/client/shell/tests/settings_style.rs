use super::*;
use crate::config::{UiGlyphsConfig, UiStyleConfig};

fn state_in_style_section() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_settings_overlay();
    let mut outcome = ClientShellInput::default();
    state.select_settings_section(ClientSettingsSection::Style, &mut outcome);
    state
}

fn selected(state: &ClientShellState) -> usize {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Settings(settings)) => settings.selected,
        _ => panic!("settings overlay is open"),
    }
}

#[test]
fn style_section_starts_on_the_current_style_and_glyphs() {
    for (style, glyphs, expected) in [
        (UiStyleConfig::Modern, UiGlyphsConfig::Unicode, 0),
        (UiStyleConfig::Modern, UiGlyphsConfig::Ascii, 1),
        (UiStyleConfig::Classic, UiGlyphsConfig::Unicode, 2),
        (UiStyleConfig::Classic, UiGlyphsConfig::Ascii, 2),
    ] {
        let mut config = Config::default();
        config.ui.style = style;
        config.ui.glyphs = glyphs;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.open_settings_overlay();
        let mut outcome = ClientShellInput::default();
        state.select_settings_section(ClientSettingsSection::Style, &mut outcome);
        assert_eq!(selected(&state), expected, "{style:?} {glyphs:?}");
    }
}

#[test]
fn moving_the_selection_previews_the_style_and_escape_restores_it() {
    let mut state = state_in_style_section();
    state.move_settings_selection(1);
    assert_eq!(state.config.ui_style, UiStyleConfig::Modern);
    assert_eq!(state.config.ui_glyphs, UiGlyphsConfig::Ascii);
    assert!(std::ptr::eq(
        state.config.chrome.glyphs,
        crate::ui::chrome::GlyphSet::for_config(UiGlyphsConfig::Ascii)
    ));

    state.move_settings_selection(1);
    assert_eq!(state.config.ui_style, UiStyleConfig::Classic);
    assert!(!state.config.chrome.is_modern());

    state.cancel_settings_overlay();
    assert_eq!(state.config.ui_style, UiStyleConfig::Modern);
    assert_eq!(state.config.ui_glyphs, UiGlyphsConfig::Unicode);
    assert!(state.config.chrome.is_modern());
    assert!(state.overlay.is_none());
}

#[test]
fn choosing_classic_leaves_the_glyph_setting_alone() {
    let mut config = Config::default();
    config.ui.glyphs = UiGlyphsConfig::Ascii;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.open_settings_overlay();
    let mut outcome = ClientShellInput::default();
    state.select_settings_section(ClientSettingsSection::Style, &mut outcome);
    state.select_settings_choice(2);
    assert_eq!(state.config.ui_style, UiStyleConfig::Classic);
    assert_eq!(state.config.ui_glyphs, UiGlyphsConfig::Ascii);
}

#[test]
fn selection_is_clamped_to_the_three_style_choices() {
    let mut state = state_in_style_section();
    state.move_settings_selection(10);
    assert_eq!(selected(&state), 2);
    state.move_settings_selection(-10);
    assert_eq!(selected(&state), 0);
}

#[test]
fn the_style_tab_is_listed_and_fits_the_settings_overlay() {
    assert!(ClientSettingsSection::available(true).contains(&ClientSettingsSection::Style));
    let mut state = state_in_style_section();
    state.compose(106, 40).unwrap();
    assert!(state
        .hits
        .settings_tabs
        .iter()
        .any(|(_, section)| *section == ClientSettingsSection::Style));
    // Every section keeps a clickable tab at the overlay's fixed width.
    assert_eq!(
        state.hits.settings_tabs.len(),
        ClientSettingsSection::available(true).len()
    );
    assert_eq!(state.hits.settings_choices.len(), 3);
}

#[test]
fn classic_has_no_style_tab_so_its_settings_screen_is_unchanged() {
    assert!(!ClientSettingsSection::available(false).contains(&ClientSettingsSection::Style));
    assert_eq!(
        ClientSettingsSection::available(false),
        ClientSettingsSection::ALL
    );

    let mut config = Config::default();
    config.ui.style = UiStyleConfig::Classic;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_settings_overlay();
    state.compose(106, 40).unwrap();
    assert_eq!(
        state.hits.settings_tabs.len(),
        ClientSettingsSection::ALL.len()
    );
    // Tabbing through every section never reaches `style`.
    for _ in 0..ClientSettingsSection::ALL.len() {
        state.handle_input_bytes(b"\t");
        assert!(!matches!(
            state.overlay,
            Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                section: ClientSettingsSection::Style,
                ..
            }))
        ));
    }
}

#[test]
fn modern_settings_tabs_remain_clickable_at_mobile_width() {
    for width in [44, 64, 80, 106] {
        let mut state = state_in_style_section();
        state.compose(width, 30).unwrap();
        assert_eq!(
            state.hits.settings_tabs.len(),
            ClientSettingsSection::available(true).len()
        );
        for (rect, _) in &state.hits.settings_tabs {
            assert!(rect.width > 0 && rect.right() <= width);
        }
        let (rect, _) = state
            .hits
            .settings_tabs
            .iter()
            .find(|(_, section)| *section == ClientSettingsSection::Style)
            .unwrap();
        let point = (rect.x, rect.y);
        state.select_settings_section(
            ClientSettingsSection::Theme,
            &mut ClientShellInput::default(),
        );
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: point.0,
            row: point.1,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                section: ClientSettingsSection::Style,
                ..
            }))
        ));
    }
}
