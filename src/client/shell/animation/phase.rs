//! Pure easing and color math shared by the animation clock and renderers.
use std::time::Duration;

use ratatui::style::Color;

use crate::app::state::Palette;

/// Progress through `duration` in `[0.0, 1.0]`.
pub(crate) fn linear_progress_phase(elapsed: Duration, duration: Duration) -> f32 {
    let total = duration.as_secs_f32();
    if total <= f32::EPSILON {
        return 1.0;
    }
    (elapsed.as_secs_f32() / total).clamp(0.0, 1.0)
}

/// Remaining intensity of an effect that decays linearly over `duration`.
pub(crate) fn linear_decay_phase(elapsed: Duration, duration: Duration) -> f32 {
    1.0 - linear_progress_phase(elapsed, duration)
}

/// Position within a repeating `period` in `[0.0, 1.0)`.
pub(crate) fn linear_cycle_phase(elapsed: Duration, period: Duration) -> f32 {
    let period_secs = period.as_secs_f32();
    if period_secs <= f32::EPSILON {
        return 0.0;
    }
    (elapsed.as_secs_f32() / period_secs).fract()
}

/// Triangle wave over `period`: rises from 0 to 1 and back to 0.
pub(crate) fn triangle_wave_phase(elapsed: Duration, period: Duration) -> f32 {
    let cycle = linear_cycle_phase(elapsed, period);
    if cycle < 0.5 {
        cycle * 2.0
    } else {
        (1.0 - cycle) * 2.0
    }
}

/// Linear RGB blend from `from` toward `to`. Non-RGB colors cannot be blended,
/// so they switch at the midpoint instead.
pub(crate) fn blend_color(from: Color, to: Color, factor: f32, palette: &Palette) -> Color {
    let factor = factor.clamp(0.0, 1.0);
    if factor <= 0.0 {
        return from;
    }
    if factor >= 1.0 {
        return to;
    }
    let (Some((r1, g1, b1)), Some((r2, g2, b2))) =
        (color_to_rgb(from, palette), color_to_rgb(to, palette))
    else {
        return if factor >= 0.5 { to } else { from };
    };
    let lerp = |a: u8, b: u8| -> u8 {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * factor)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(lerp(r1, r2), lerp(g1, g2), lerp(b1, b2))
}

fn color_to_rgb(color: Color, palette: &Palette) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Reset => match palette.panel_bg {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            _ => None,
        },
        _ => None,
    }
}
