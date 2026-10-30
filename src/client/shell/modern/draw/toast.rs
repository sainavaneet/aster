//! Modern toasts: no box, a one-cell bar in the event's state color, the state
//! glyph, a bold title and a muted body. The same slide and fade phases as the
//! classic card, widths measured in display cells (so CJK and emoji titles size
//! the card correctly), and nothing allocates: every piece is a borrowed write.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Clear, Widget},
};

use crate::app::state::CopyFeedback;
use crate::client::shell::animation::{blend_color, NotificationTransitionPhase};
use crate::client::shell::modern::model::team::PaneAttention;
use crate::client::shell::notifications::card_rect;
use crate::client::shell::ClientShellConfig;
use crate::config::{ToastAsterPosition, ToastClipboardPosition};
use crate::protocol::{SemanticNotification, SemanticNotificationKind};
use crate::ui::chrome::draw::{display_width, put_elided, put_segment_inline};
use crate::ui::chrome::state::StateKind;

/// Widest a toast grows; longer text is cut with an ellipsis.
const MAX_WIDTH: u16 = 72;
/// Cells before the title: bar, gap, glyph, gap.
const LEAD: u16 = 4;
/// Blank cell after the text.
const TRAIL: u16 = 1;

/// The state a notification is shown in. `approval` splits "needs attention"
/// into the approval and blocked looks the sidebar already uses.
pub(in crate::client::shell) const fn event_state(
    kind: SemanticNotificationKind,
    approval: bool,
) -> StateKind {
    match kind {
        SemanticNotificationKind::NeedsAttention if approval => StateKind::Approval,
        SemanticNotificationKind::NeedsAttention => StateKind::Blocked,
        SemanticNotificationKind::Finished | SemanticNotificationKind::UpdateInstalled => {
            StateKind::Done
        }
        SemanticNotificationKind::Custom => StateKind::Idle,
    }
}

/// `event_state` for a delivered event: a "needs attention" event whose pane
/// waits on an approval shows the approval look. One borrowed map lookup, for
/// the single visible toast.
pub(in crate::client::shell) fn visible_state(
    event: &SemanticNotification,
    attention: Option<&PaneAttention>,
) -> StateKind {
    let approval = event.pane_id.as_deref().is_some_and(|pane_id| {
        attention.is_some_and(|attention| attention.state(pane_id) == Some(StateKind::Approval))
    });
    event_state(event.kind, approval)
}

struct Look {
    background: ratatui::style::Color,
    bar: Style,
    glyph: Style,
    title: Style,
    body: Style,
    gutter: &'static str,
    state_glyph: &'static str,
    ellipsis: &'static str,
}

fn look(config: &ClientShellConfig, state: StateKind, phase: NotificationTransitionPhase) -> Look {
    let chrome = &config.chrome;
    let roles = &chrome.roles;
    let palette = &config.palette;
    let opacity = phase.opacity.clamp(0.15, 1.0);
    let background = blend_color(palette.panel_bg, palette.surface0, opacity, palette);
    let fade = |color| blend_color(background, color, opacity, palette);
    let state_color = fade(roles.state(state));
    let glyph = Style::default().fg(state_color).bg(background);
    Look {
        background,
        bar: Style::default().fg(state_color).bg(background),
        glyph: if roles.bold_states {
            glyph.add_modifier(Modifier::BOLD)
        } else {
            glyph
        },
        title: Style::default()
            .fg(fade(roles.text_strong))
            .bg(background)
            .add_modifier(if opacity >= 0.45 {
                Modifier::BOLD
            } else {
                Modifier::DIM
            }),
        body: Style::default().fg(fade(roles.text_muted)).bg(background),
        gutter: chrome.glyphs.role_gutter,
        state_glyph: chrome.glyphs.state(state),
        ellipsis: chrome.glyphs.ellipsis,
    }
}

/// Draws the bar, gap, state glyph and gap at `(x, y)`; returns the next column.
fn put_lead(buffer: &mut Buffer, rect: Rect, y: u16, look: &Look, with_glyph: bool) -> u16 {
    let right = rect.right();
    let blank = Style::default().bg(look.background);
    let mut x = put_segment_inline(buffer, rect.x, y, right, look.gutter, look.bar);
    x = put_segment_inline(buffer, x, y, right, " ", blank);
    if with_glyph {
        x = put_segment_inline(buffer, x, y, right, look.state_glyph, look.glyph);
    } else {
        x = put_segment_inline(buffer, x, y, right, " ", blank);
    }
    put_segment_inline(buffer, x, y, right, " ", blank)
}

fn card_width(title: &str, body: &str, available: u16) -> u16 {
    let text = display_width(title).max(display_width(body));
    LEAD.saturating_add(text)
        .saturating_add(TRAIL)
        .min(MAX_WIDTH)
        .min(available)
}

/// The toast card for the desktop layout. Returns its rect (the click target).
// Geometry, event text and transition state are borrowed independently.
#[allow(clippy::too_many_arguments)]
pub(in crate::client::shell) fn draw_card(
    buffer: &mut Buffer,
    area: Rect,
    state: StateKind,
    title: &str,
    body: &str,
    position: ToastAsterPosition,
    top_offset: u16,
    config: &ClientShellConfig,
    phase: NotificationTransitionPhase,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    let height: u16 = if body.is_empty() { 1 } else { 2 }.min(area.height);
    let width = card_width(title, body, area.width);
    let rect = card_rect(area, width, height, position, top_offset, phase);
    let look = look(config, state, phase);
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(look.background));
    let right = rect.right();
    // A trailing blank keeps the text off the card's edge.
    let text_right = right.saturating_sub(TRAIL).max(rect.x);
    let x = put_lead(buffer, rect, rect.y, &look, true);
    put_elided(
        buffer,
        x,
        rect.y,
        text_right,
        title,
        look.ellipsis,
        look.title,
    );
    if height > 1 {
        let y = rect.y + 1;
        let x = put_lead(buffer, rect, y, &look, false);
        put_elided(buffer, x, y, text_right, body, look.ellipsis, look.body);
    }
    rect
}

/// The one-row banner of the mobile layout: bar, glyph, title, then the body.
// Geometry, event text and transition state are borrowed independently.
#[allow(clippy::too_many_arguments)]
pub(in crate::client::shell) fn draw_banner(
    buffer: &mut Buffer,
    area: Rect,
    state: StateKind,
    title: &str,
    body: Option<&str>,
    offset_for_warning: bool,
    config: &ClientShellConfig,
    phase: NotificationTransitionPhase,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    let y = area.y
        + area
            .height
            .saturating_sub(1u16.saturating_add(u16::from(offset_for_warning)));
    let rect = Rect::new(area.x, y, area.width, 1);
    let look = look(config, state, phase);
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(look.background));
    let slid = Rect::new(
        rect.x
            .saturating_add(phase.slide_offset_cols.min(rect.width.saturating_sub(1))),
        rect.y,
        rect.width
            .saturating_sub(phase.slide_offset_cols.min(rect.width.saturating_sub(1))),
        1,
    );
    let x = put_lead(buffer, slid, slid.y, &look, true);
    let right = rect.right();
    let mut x = put_elided(buffer, x, rect.y, right, title, look.ellipsis, look.title);
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        x = put_segment_inline(buffer, x, rect.y, right, " ", look.body);
        x = put_segment_inline(
            buffer,
            x,
            rect.y,
            right,
            config.chrome.glyphs.separator,
            look.body,
        );
        x = put_segment_inline(buffer, x, rect.y, right, " ", look.body);
        put_elided(buffer, x, rect.y, right, body, look.ellipsis, look.body);
    }
    rect
}

/// Where the clipboard feedback sits: one row, sized in display cells.
pub(in crate::client::shell) fn feedback_rect(
    area: Rect,
    feedback: &CopyFeedback,
    offset_rows: u16,
    position: ToastClipboardPosition,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    let width = LEAD
        .saturating_add(display_width(&feedback.message))
        .saturating_add(TRAIL)
        .min(MAX_WIDTH)
        .min(area.width);
    let height = 1u16.min(area.height);
    let x = match position {
        ToastClipboardPosition::TopLeft | ToastClipboardPosition::BottomLeft => area.x,
        ToastClipboardPosition::TopCenter | ToastClipboardPosition::BottomCenter => {
            area.x + area.width.saturating_sub(width) / 2
        }
        ToastClipboardPosition::TopRight | ToastClipboardPosition::BottomRight => {
            area.x + area.width.saturating_sub(width)
        }
    };
    let y = match position {
        ToastClipboardPosition::TopLeft
        | ToastClipboardPosition::TopCenter
        | ToastClipboardPosition::TopRight => {
            area.y + offset_rows.min(area.height.saturating_sub(height))
        }
        ToastClipboardPosition::BottomLeft
        | ToastClipboardPosition::BottomCenter
        | ToastClipboardPosition::BottomRight => {
            area.y
                + area
                    .height
                    .saturating_sub(height.saturating_add(offset_rows))
        }
    };
    Rect::new(x, y, width, height)
}

/// Rows to push the clipboard feedback down so it does not cover `toast`.
pub(in crate::client::shell) fn feedback_offset(
    area: Rect,
    feedback: &CopyFeedback,
    base_offset: u16,
    position: ToastClipboardPosition,
    toast: Rect,
) -> u16 {
    let rect = feedback_rect(area, feedback, base_offset, position);
    let overlaps = rect.x < toast.right()
        && toast.x < rect.right()
        && rect.y < toast.bottom()
        && toast.y < rect.bottom();
    if overlaps {
        base_offset.saturating_add(toast.height)
    } else {
        base_offset
    }
}

/// "Copied" feedback in the toast look: a done bar and glyph, the message.
pub(in crate::client::shell) fn draw_feedback(
    buffer: &mut Buffer,
    area: Rect,
    feedback: &CopyFeedback,
    offset_rows: u16,
    position: ToastClipboardPosition,
    config: &ClientShellConfig,
) -> Rect {
    let rect = feedback_rect(area, feedback, offset_rows, position);
    if rect.is_empty() {
        return rect;
    }
    let look = look(
        config,
        StateKind::Done,
        NotificationTransitionPhase::SETTLED,
    );
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(look.background));
    let x = put_lead(buffer, rect, rect.y, &look, true);
    put_elided(
        buffer,
        x,
        rect.y,
        rect.right().saturating_sub(TRAIL).max(rect.x),
        &feedback.message,
        look.ellipsis,
        look.title,
    );
    rect
}

/// The config diagnostic rows, right-aligned like the classic ones but sized
/// in display cells (a non-ASCII line no longer widens the row). Returns the
/// rows drawn.
pub(in crate::client::shell) fn draw_config_diagnostic(
    buffer: &mut Buffer,
    area: Rect,
    message: &str,
    config: &ClientShellConfig,
    mut covered: impl FnMut(Rect),
) -> u16 {
    let palette = &config.palette;
    let style = Style::default()
        .fg(crate::client::shell::panel_contrast_fg(palette))
        .bg(palette.yellow)
        .add_modifier(Modifier::BOLD);
    let mut rows = 0u16;
    for (row, line) in message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(usize::from(area.height))
        .enumerate()
    {
        let cells = display_width(line).saturating_add(2);
        let width = cells.min(area.width);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width),
            area.y + row as u16,
            width,
            1,
        );
        covered(rect);
        Clear.render(rect, buffer);
        buffer.set_style(rect, style);
        put_text_gapped(buffer, rect, line, style);
        rows = rows.saturating_add(1);
    }
    rows
}

/// `line` one cell in from the left edge of `rect`, clipped one cell short of
/// the right edge.
fn put_text_gapped(buffer: &mut Buffer, rect: Rect, line: &str, style: Style) {
    let inner = rect.width.saturating_sub(2);
    let x = rect.x.saturating_add(1);
    put_segment_inline(buffer, x, rect.y, x.saturating_add(inner), line, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_map_to_the_shared_state_vocabulary() {
        use SemanticNotificationKind::*;
        assert_eq!(event_state(NeedsAttention, false), StateKind::Blocked);
        assert_eq!(event_state(NeedsAttention, true), StateKind::Approval);
        assert_eq!(event_state(Finished, false), StateKind::Done);
        assert_eq!(event_state(UpdateInstalled, false), StateKind::Done);
        assert_eq!(event_state(Custom, false), StateKind::Idle);
    }

    #[test]
    fn card_width_counts_display_cells_not_bytes() {
        // Six CJK characters are twelve cells wide but eighteen bytes long.
        assert_eq!(card_width("日本語日本語", "", 80), LEAD + 12 + TRAIL);
        assert_eq!(card_width("ab", "wider body", 80), LEAD + 10 + TRAIL);
        assert_eq!(card_width("x", "", 3), 3);
        assert_eq!(card_width(&"w".repeat(200), "", 200), MAX_WIDTH);
    }
}
