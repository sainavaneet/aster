//! The activity suffix of a modern tab: one chip per role and the counts of
//! agents working or needing you, in the same glyphs as the sidebar.

use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::client::shell::modern::model::tabs::{AttentionSummary, TabActivity};
use crate::client::shell::{panel_contrast_fg, ClientShellConfig};
use crate::ui::chrome::draw::{put_number, put_segment_inline as put_segment};

/// Draws `activity` inside `area` (the tab's right part, `suffix_width` cells
/// wide, its first cell the gap after the label). On the focused tab every
/// glyph takes the tab's contrast color: a role tint or state color on the
/// accent fill would not be legible in every theme.
pub(in crate::client::shell) fn render_suffix(
    buffer: &mut Buffer,
    area: Rect,
    activity: &TabActivity,
    focused: bool,
    config: &ClientShellConfig,
) {
    let chrome = &config.chrome;
    let roles = &chrome.roles;
    let palette = &config.palette;
    let background = if focused {
        palette.accent
    } else {
        palette.surface0
    };
    let on = |color| {
        let color = if focused {
            panel_contrast_fg(palette)
        } else {
            color
        };
        Style::default().fg(color).bg(background)
    };
    let right = area.right();
    let mut x = area.x.saturating_add(1);
    for chip in &activity.chips[..usize::from(activity.chip_len)] {
        let glyph = if chip.working {
            chrome.glyphs.chip_on
        } else {
            chrome.glyphs.chip_off
        };
        x = put_segment(
            buffer,
            x,
            area.y,
            right,
            glyph,
            on(roles.tint(usize::from(chip.tint))),
        );
    }
    // Most urgent first, the same order as the attention chip.
    let parts = [
        (activity.blocked, chrome.glyphs.blocked, roles.state_blocked),
        (
            activity.approval,
            chrome.glyphs.approval,
            roles.state_approval,
        ),
        (activity.working, chrome.glyphs.working, roles.state_working),
    ];
    for (count, glyph, color) in parts {
        if count == 0 {
            continue;
        }
        x = put_segment(buffer, x, area.y, right, " ", on(roles.text_muted));
        let style = on(color);
        x = put_segment(buffer, x, area.y, right, glyph, style);
        x = put_number(buffer, x, area.y, right, usize::from(count), style);
    }
}

/// The chip at the tab bar's right edge: `▲1 ⚑1 ◐3`, most urgent first, parts
/// with a zero count omitted.
pub(in crate::client::shell) fn render_attention(
    buffer: &mut Buffer,
    area: Rect,
    summary: &AttentionSummary,
    config: &ClientShellConfig,
) {
    let chrome = &config.chrome;
    let roles = &chrome.roles;
    let background = config.palette.panel_bg;
    let right = area.right();
    let mut x = area.x;
    let parts = [
        (summary.blocked, chrome.glyphs.blocked, roles.state_blocked),
        (
            summary.approval,
            chrome.glyphs.approval,
            roles.state_approval,
        ),
        (summary.working, chrome.glyphs.working, roles.state_working),
    ];
    let mut first = true;
    for (count, glyph, color) in parts {
        if count == 0 {
            continue;
        }
        if !first {
            x = put_segment(
                buffer,
                x,
                area.y,
                right,
                " ",
                Style::default().bg(background),
            );
        }
        first = false;
        let style = Style::default().fg(color).bg(background);
        x = put_segment(buffer, x, area.y, right, glyph, style);
        x = put_number(buffer, x, area.y, right, usize::from(count), style);
    }
}
