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
        // Focused rows sit on `active_row_bg`, so colors must read on it too.
        let backgrounds = [palette.panel_bg, palette.sidebar_bg, palette.active_row_bg].map(rgb);
        let measurable = backgrounds.iter().all(Option::is_some)
            && [
                palette.yellow,
                palette.red,
                palette.peach,
                palette.green,
                palette.overlay1,
            ]
            .into_iter()
            .all(|color| rgb(color).is_some());
        let fit = |color: Color| ensure_contrast(color, &backgrounds, MIN_STATE_CONTRAST);
        Self {
            state_working: fit(palette.yellow),
            state_blocked: fit(palette.red),
            state_approval: fit(palette.peach),
            state_done: fit(palette.green),
            state_idle: fit(palette.overlay1),
            focus: fit(palette.accent),
            flow: fit(palette.mauve),
            parallel: fit(palette.teal),
            role_tint: [
                fit(palette.accent),
                fit(palette.mauve),
                fit(palette.teal),
                fit(palette.overlay1),
            ],
            text_strong: fit(palette.text),
            text_body: fit(palette.subtext0),
            text_muted: fit(palette.overlay0),
            text_subtle: fit(palette.overlay1),
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

fn luminance64((r, g, b): (u8, u8, u8)) -> f64 {
    let linear = |v: u8| {
        let s = f64::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// Intersect the exact allowed luminance intervals for every background.
fn feasible_luminances(backgrounds: &[(u8, u8, u8)], minimum: f64) -> Vec<(f64, f64)> {
    let mut ranges: Vec<(f64, f64)> = vec![(0.0, 1.0)];
    for &background in backgrounds {
        let b = luminance64(background);
        let dark = (b + 0.05) / minimum - 0.05;
        let light = minimum * (b + 0.05) - 0.05;
        ranges = ranges
            .into_iter()
            .flat_map(|(lo, hi)| {
                [(lo, hi.min(dark)), (lo.max(light), hi)]
                    .into_iter()
                    .filter(|&(a, b)| a <= b && a >= 0.0 && b <= 1.0)
            })
            .collect();
    }
    ranges
}

/// Preserve a passing color, otherwise fit its luminance to the intersection
/// allowed by all backgrounds. Enumerate every rounded color on the two
/// hue-preserving tint/shade paths, rather than sampling those paths at 5%.
/// A rare exact 8-bit RGB search handles feasible bands neither path can hit.
/// This runs only on palette changes, never in rendering.
pub(crate) fn ensure_contrast(
    color: Color,
    backgrounds: &[Option<(u8, u8, u8)>],
    minimum: f32,
) -> Color {
    let Some(base) = rgb(color) else { return color };
    let backgrounds: Vec<_> = backgrounds.iter().flatten().copied().collect();
    if backgrounds.is_empty() || !minimum.is_finite() || minimum <= 1.0 {
        return color;
    }
    let score = |candidate| {
        backgrounds
            .iter()
            .map(|&bg| contrast_ratio(candidate, bg))
            .fold(f32::INFINITY, f32::min)
    };
    if score(base) >= minimum {
        return color;
    }
    let ranges = feasible_luminances(&backgrounds, f64::from(minimum));
    let distance = |c: (u8, u8, u8)| {
        [(c.0, base.0), (c.1, base.1), (c.2, base.2)]
            .into_iter()
            .map(|(a, b)| (i32::from(a) - i32::from(b)).pow(2) as u32)
            .sum::<u32>()
    };
    let mut best = (score(base), base);
    let mut closest: Option<(u32, (u8, u8, u8))> = None;
    let consider =
        |candidate, closest: &mut Option<(u32, (u8, u8, u8))>, best: &mut (f32, (u8, u8, u8))| {
            let value = score(candidate);
            if value > best.0 {
                *best = (value, candidate);
            }
            let l = luminance64(candidate);
            if value >= minimum && ranges.iter().any(|&(a, b)| a <= l && l <= b) {
                let d = distance(candidate);
                if closest.is_none_or(|(old, _)| d < old) {
                    *closest = Some((d, candidate));
                }
            }
        };
    for target in [0u8, 255] {
        let mut breaks = vec![0.0, 1.0];
        for channel in [base.0, base.1, base.2] {
            let delta = f64::from(target) - f64::from(channel);
            if delta == 0.0 {
                continue;
            }
            for level in 0..255 {
                let t = (f64::from(level) + 0.5 - f64::from(channel)) / delta;
                if t > 0.0 && t < 1.0 {
                    breaks.push(t);
                }
            }
        }
        breaks.sort_unstable_by(f64::total_cmp);
        breaks.dedup();
        let tint = |t: f64| {
            let mix = |v: u8| {
                (f64::from(v) + (f64::from(target) - f64::from(v)) * t)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            (mix(base.0), mix(base.1), mix(base.2))
        };
        for pair in breaks.windows(2) {
            consider(tint(pair[0]), &mut closest, &mut best);
            consider(tint((pair[0] + pair[1]) * 0.5), &mut closest, &mut best);
        }
        consider((target, target, target), &mut closest, &mut best);
    }
    if let Some((_, (r, g, b))) = closest {
        return Color::Rgb(r, g, b);
    }
    for level in 0..=255 {
        consider((level, level, level), &mut closest, &mut best);
    }
    if let Some((_, (r, g, b))) = closest {
        return Color::Rgb(r, g, b);
    }
    if !ranges.is_empty() {
        // Luminance is additive and monotone in each channel. For every R/G
        // pair, binary search exactly the allowed B interval, then choose the
        // B nearest to the base. This covers all representable RGB candidates.
        let linear: [f64; 256] = std::array::from_fn(|v| luminance64((v as u8, v as u8, v as u8)));
        for r in 0..=255u8 {
            for g in 0..=255u8 {
                let rg = 0.2126 * linear[usize::from(r)] + 0.7152 * linear[usize::from(g)];
                for &(lo, hi) in &ranges {
                    let low = linear.partition_point(|&v| rg + 0.0722 * v < lo);
                    let high = linear.partition_point(|&v| rg + 0.0722 * v <= hi);
                    if low >= high {
                        continue;
                    }
                    let b = usize::from(base.2).clamp(low, high - 1);
                    for b in [b, low, high - 1] {
                        let candidate = (r, g, b as u8);
                        if score(candidate) >= minimum {
                            let d = distance(candidate);
                            if closest.is_none_or(|(old, _)| d < old) {
                                closest = Some((d, candidate));
                            }
                        }
                    }
                }
            }
        }
    }
    let (_, (r, g, b)) = closest.unwrap_or((0, best.1));
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(name: &str) -> Palette {
        Palette::from_name(name).unwrap_or_else(|| panic!("theme {name}"))
    }

    /// Every derived color role that carries meaning (not rules or fills).
    fn measured_roles(roles: &ChromeRoles) -> Vec<(&'static str, Color)> {
        let mut measured = vec![
            ("state_working", roles.state_working),
            ("state_blocked", roles.state_blocked),
            ("state_approval", roles.state_approval),
            ("state_done", roles.state_done),
            ("state_idle", roles.state_idle),
            ("focus", roles.focus),
            ("flow", roles.flow),
            ("parallel", roles.parallel),
            ("text_strong", roles.text_strong),
            ("text_body", roles.text_body),
            ("text_muted", roles.text_muted),
            ("text_subtle", roles.text_subtle),
        ];
        measured.extend(roles.role_tint.iter().map(|tint| ("role_tint", *tint)));
        measured
    }

    fn assert_roles_legible(name: &str, palette: &Palette) {
        let roles = ChromeRoles::derive(palette);
        for background in [palette.panel_bg, palette.sidebar_bg, palette.active_row_bg]
            .into_iter()
            .filter_map(rgb)
        {
            for (role, color) in measured_roles(&roles) {
                let Some(color) = rgb(color) else { continue };
                assert!(
                    contrast_ratio(color, background) >= MIN_STATE_CONTRAST,
                    "{name}: {role} {color:?} on {background:?} = {}",
                    contrast_ratio(color, background)
                );
            }
        }
    }

    #[test]
    fn every_builtin_theme_keeps_every_role_legible() {
        for name in crate::config::THEME_NAMES {
            assert_roles_legible(name, &palette(name));
        }
    }

    #[test]
    fn custom_mid_grey_backgrounds_keep_every_role_legible() {
        // Mid greys are where "pick white when luminance < 0.5" fails: #b0b0b0
        // is below 0.5 relative luminance yet white on it is only ~2.2:1.
        for level in (0u8..=255).step_by(5).chain([0xb0, 0x77, 0x80, 0x99]) {
            let grey = Color::Rgb(level, level, level);
            for base in ["catppuccin", "catppuccin-latte", "nord"] {
                let mut custom = palette(base);
                custom.panel_bg = grey;
                custom.sidebar_bg = grey;
                custom.active_row_bg = grey;
                assert_roles_legible(&format!("{base} on #{level:02x}"), &custom);
            }
        }
    }

    #[test]
    fn latte_idle_and_b0b0b0_working_reach_the_floor() {
        let latte = palette("catppuccin-latte");
        let idle = rgb(ChromeRoles::derive(&latte).state_idle).unwrap();
        assert!(contrast_ratio(idle, rgb(latte.panel_bg).unwrap()) >= MIN_STATE_CONTRAST);

        let mut grey = palette("catppuccin");
        grey.panel_bg = Color::Rgb(0xb0, 0xb0, 0xb0);
        grey.sidebar_bg = Color::Rgb(0xb0, 0xb0, 0xb0);
        grey.active_row_bg = Color::Rgb(0xb0, 0xb0, 0xb0);
        let working = rgb(ChromeRoles::derive(&grey).state_working).unwrap();
        assert!(contrast_ratio(working, (0xb0, 0xb0, 0xb0)) >= MIN_STATE_CONTRAST);
    }

    #[test]
    fn mixed_light_and_dark_backgrounds_use_a_readable_intermediate() {
        let backgrounds = [Some((0, 0, 0)), Some((255, 255, 255))];
        for base in [Color::Rgb(255, 255, 0), Color::Rgb(0, 0, 255)] {
            let fitted = rgb(ensure_contrast(base, &backgrounds, MIN_STATE_CONTRAST)).unwrap();
            for background in backgrounds.into_iter().flatten() {
                assert!(contrast_ratio(fitted, background) >= MIN_STATE_CONTRAST);
            }
        }
    }

    #[test]
    fn indexed_state_color_requests_bold_even_with_rgb_backgrounds() {
        let mut custom = palette("catppuccin");
        custom.yellow = Color::Indexed(3);
        let roles = ChromeRoles::derive(&custom);
        assert_eq!(roles.state_working, Color::Indexed(3));
        assert!(roles.bold_states);
    }

    #[test]
    fn colors_that_already_pass_are_left_alone() {
        let mut palette = palette("catppuccin");
        // Make every background measurable; Reset must request the bold fallback.
        palette.sidebar_bg = palette.panel_bg;
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

#[cfg(test)]
mod phase6_tests {
    use super::*;
    #[test]
    fn interval_fitting_preserves_valid_inputs_and_handles_impossible_bands() {
        assert_eq!(
            ensure_contrast(Color::Reset, &[Some((0, 0, 0))], 3.0),
            Color::Reset
        );
        let base = Color::Rgb(220, 200, 180);
        assert_eq!(ensure_contrast(base, &[None], 3.0), base);
        assert_eq!(ensure_contrast(base, &[Some((0, 0, 0))], 3.0), base);
        assert!(feasible_luminances(&[(0, 0, 0), (255, 255, 255)], 5.0).is_empty());
        let fitted = rgb(ensure_contrast(
            base,
            &[Some((0, 0, 0)), Some((255, 255, 255))],
            5.0,
        ))
        .unwrap();
        let worst = |c| contrast_ratio(c, (0, 0, 0)).min(contrast_ratio(c, (255, 255, 255)));
        assert!(worst(fitted) >= worst(rgb(base).unwrap()));
    }

    #[test]
    fn every_quantized_grey_feasible_band_has_a_passing_fit() {
        for dark in [0, 9, 40, 80] {
            for light in [150, 175, 210, 255] {
                let bgs = [Some((dark, dark, dark)), Some((light, light, light))];
                let passes = |c| bgs.iter().flatten().all(|&b| contrast_ratio(c, b) >= 3.0);
                if !(0..=255u8).any(|v| passes((v, v, v))) {
                    continue;
                }
                for base in [(249, 226, 175), (30, 190, 240), (180, 80, 130)] {
                    let fitted = rgb(ensure_contrast(
                        Color::Rgb(base.0, base.1, base.2),
                        &bgs,
                        3.0,
                    ))
                    .unwrap();
                    assert!(passes(fitted), "{dark}/{light}/{base:?}: {fitted:?}");
                }
            }
        }
    }

    #[test]
    fn narrow_feasible_luminance_interval_is_not_skipped() {
        let bgs = [Some((9, 9, 9)), Some((175, 175, 175))];
        assert!(bgs
            .iter()
            .flatten()
            .all(|bg| contrast_ratio((93, 93, 93), *bg) >= 3.0));
        let fitted = rgb(ensure_contrast(Color::Rgb(249, 226, 175), &bgs, 3.0)).unwrap();
        assert!(
            bgs.iter()
                .flatten()
                .all(|bg| contrast_ratio(fitted, *bg) >= 3.0),
            "{fitted:?}"
        );
    }
}
