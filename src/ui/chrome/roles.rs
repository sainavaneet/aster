//! Semantic color roles of the modern style, derived once from a `Palette`.
//!
//! No new theme keys: every role is a palette color, nudged only as far as
//! needed to reach a legible contrast against the theme's backgrounds. Colors
//! that are not `Rgb` (the `terminal` theme, `Reset` backgrounds) cannot be
//! measured and keep their palette value; `bold_states` then asks the drawer
//! to add `BOLD` so shape and weight carry the state.

use ratatui::style::Color;

use super::state::StateKind;
use crate::app::state::Palette;

/// Minimum contrast for state glyphs against a measurable background.
pub(crate) const MIN_STATE_CONTRAST: f32 = 3.0;
const ROLE_TINTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChromeRoles {
    pub(crate) state_working: Color,
    pub(crate) state_blocked: Color,
    pub(crate) state_approval: Color,
    pub(crate) state_done: Color,
    pub(crate) state_idle: Color,
    pub(crate) focus: Color,
    pub(crate) flow: Color,
    pub(crate) parallel: Color,
    pub(crate) role_tint: [Color; ROLE_TINTS],
    pub(crate) text_strong: Color,
    pub(crate) text_body: Color,
    pub(crate) text_muted: Color,
    pub(crate) text_subtle: Color,
    pub(crate) rule: Color,
    pub(crate) row_focus_bg: Color,
    /// True when state colors could not be contrast-checked, so drawers add
    /// `BOLD` to state glyphs.
    pub(crate) bold_states: bool,
}

impl ChromeRoles {
    pub(crate) fn derive(palette: &Palette) -> Self {
        let backgrounds = [palette.panel_bg, palette.sidebar_bg].map(rgb);
        let measurable = backgrounds.iter().any(Option::is_some);
        let fit = |color: Color| ensure_contrast(color, &backgrounds, MIN_STATE_CONTRAST);
        Self {
            state_working: fit(palette.yellow),
            state_blocked: fit(palette.red),
            state_approval: fit(palette.peach),
            state_done: fit(palette.green),
            state_idle: palette.overlay1,
            focus: palette.accent,
            flow: fit(palette.mauve),
            parallel: fit(palette.teal),
            role_tint: [
                palette.accent,
                palette.mauve,
                palette.teal,
                palette.overlay1,
            ],
            text_strong: palette.text,
            text_body: palette.subtext0,
            text_muted: palette.overlay0,
            text_subtle: palette.overlay1,
            rule: match palette.sidebar_bg {
                Color::Reset => palette.surface1,
                _ => palette.surface_dim,
            },
            row_focus_bg: palette.active_row_bg,
            bold_states: !measurable,
        }
    }

    /// The color of a state glyph.
    pub(crate) fn state(&self, state: StateKind) -> Color {
        match state {
            StateKind::Working => self.state_working,
            StateKind::Blocked | StateKind::Failed => self.state_blocked,
            StateKind::Approval => self.state_approval,
            StateKind::Done => self.state_done,
            StateKind::Idle | StateKind::Queued | StateKind::Exited | StateKind::Unknown => {
                self.state_idle
            }
        }
    }

    /// The identity tint of the `index`th role, cycling.
    pub(crate) fn tint(&self, index: usize) -> Color {
        self.role_tint[index % ROLE_TINTS]
    }
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn channel(value: u8) -> f32 {
    let value = f32::from(value) / 255.0;
    if value <= 0.03928 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG contrast ratio of two colors, 1.0 to 21.0.
pub(crate) fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (high, low) = if la >= lb { (la, lb) } else { (lb, la) };
    (high + 0.05) / (low + 0.05)
}

fn mix(from: (u8, u8, u8), to: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let lerp = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (lerp(from.0, to.0), lerp(from.1, to.1), lerp(from.2, to.2))
}

/// `color` unchanged when it already reaches `minimum` against every
/// measurable background (or cannot be measured); otherwise mixed toward black
/// on light backgrounds and white on dark ones in 5% steps until it does.
pub(crate) fn ensure_contrast(
    color: Color,
    backgrounds: &[Option<(u8, u8, u8)>],
    minimum: f32,
) -> Color {
    let Some(base) = rgb(color) else {
        return color;
    };
    let mut fitted = base;
    for background in backgrounds.iter().flatten() {
        let target = if relative_luminance(*background) > 0.5 {
            (0, 0, 0)
        } else {
            (255, 255, 255)
        };
        let mut step = 0u8;
        while contrast_ratio(fitted, *background) < minimum && step < 20 {
            step += 1;
            fitted = mix(base, target, f32::from(step) * 0.05);
        }
    }
    Color::Rgb(fitted.0, fitted.1, fitted.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(name: &str) -> Palette {
        Palette::from_name(name).unwrap_or_else(|| panic!("theme {name}"))
    }

    #[test]
    fn every_builtin_theme_meets_the_state_contrast_floor() {
        for name in crate::config::THEME_NAMES {
            let palette = palette(name);
            let roles = ChromeRoles::derive(&palette);
            for background in [palette.panel_bg, palette.sidebar_bg]
                .into_iter()
                .filter_map(rgb)
            {
                for color in [
                    roles.state_working,
                    roles.state_blocked,
                    roles.state_approval,
                    roles.state_done,
                    roles.flow,
                    roles.parallel,
                ] {
                    let Some(color) = rgb(color) else { continue };
                    assert!(
                        contrast_ratio(color, background) >= MIN_STATE_CONTRAST - 0.05,
                        "{name}: {color:?} on {background:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn colors_that_already_pass_are_left_alone() {
        let palette = palette("catppuccin");
        let roles = ChromeRoles::derive(&palette);
        assert_eq!(roles.state_working, palette.yellow);
        assert_eq!(roles.focus, palette.accent);
        assert!(!roles.bold_states);
    }

    #[test]
    fn light_themes_darken_low_contrast_colors() {
        let palette = palette("catppuccin-latte");
        let roles = ChromeRoles::derive(&palette);
        let before = rgb(palette.yellow).unwrap();
        let after = rgb(roles.state_working).unwrap();
        let background = rgb(palette.panel_bg).unwrap();
        assert!(contrast_ratio(after, background) > contrast_ratio(before, background));
    }

    #[test]
    fn unmeasurable_themes_keep_palette_colors_and_ask_for_bold() {
        let palette = palette("terminal");
        let roles = ChromeRoles::derive(&palette);
        assert_eq!(roles.state_working, palette.yellow);
        assert!(roles.bold_states);
    }

    #[test]
    fn role_tints_cycle_and_never_overlap_state_colors_by_index() {
        let roles = ChromeRoles::derive(&palette("catppuccin"));
        assert_eq!(roles.tint(0), roles.tint(ROLE_TINTS));
        assert_ne!(roles.tint(0), roles.tint(1));
    }
}
