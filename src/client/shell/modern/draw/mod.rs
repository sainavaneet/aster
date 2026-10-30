//! Allocation-free drawing of the modern sidebar. Everything here borrows the
//! precomputed models and writes cells; nothing formats, clones or collects.

pub(in crate::client::shell) mod flow;
pub(in crate::client::shell) mod lanes;
pub(in crate::client::shell) mod pane_header;
pub(in crate::client::shell) mod status;
pub(in crate::client::shell) mod tabs;
pub(in crate::client::shell) mod team;
pub(in crate::client::shell) mod toast;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::api::schema::AgentStatus;
use crate::client::shell::animation::{blend_color, AgentRowAnimationPhase};
use crate::client::shell::team_sidebar::TeamRenderContext;
use crate::ui::chrome::draw::{display_width, put_number, put_segment_inline as put_segment};
use crate::ui::chrome::state::{StateCounts, StateKind};

/// States a role header rolls up, most urgent first.
const ROLLUP: [StateKind; 5] = [
    StateKind::Approval,
    StateKind::Blocked,
    StateKind::Working,
    StateKind::Queued,
    StateKind::Idle,
];

pub(super) fn agent_status(state: StateKind) -> AgentStatus {
    match state {
        StateKind::Working => AgentStatus::Working,
        StateKind::Blocked | StateKind::Approval | StateKind::Failed => AgentStatus::Blocked,
        StateKind::Done => AgentStatus::Done,
        StateKind::Idle | StateKind::Queued => AgentStatus::Idle,
        StateKind::Exited | StateKind::Unknown => AgentStatus::Unknown,
    }
}

/// A state's glyph and animated color for the frame: the shared spinner for a
/// working agent, a glyph-only pulse while something needs you, a brief flash
/// on a status change. Static under `ui.animations` reduced or off.
pub(super) fn state_glyph(
    ctx: &TeamRenderContext<'_>,
    state: StateKind,
    pane_id: &str,
) -> (&'static str, Style, AgentRowAnimationPhase) {
    let chrome = &ctx.config.chrome;
    let palette = &ctx.config.palette;
    let phase = ctx
        .animation
        .agent_row(ctx.endpoint_id, pane_id, agent_status(state));
    let glyph = if state == StateKind::Working {
        chrome.glyphs.working_frame(phase.spinner_index)
    } else {
        chrome.glyphs.state(state)
    };
    let mut color = chrome.roles.state(state);
    if matches!(state, StateKind::Blocked | StateKind::Approval) && phase.blocked_pulse > 0.0 {
        color = blend_color(color, palette.surface1, phase.blocked_pulse * 0.65, palette);
    }
    if phase.status_flash > 0.0 {
        color = blend_color(color, palette.text, phase.status_flash * 0.75, palette);
    }
    (glyph, state_style(ctx, color), phase)
}

pub(super) fn state_style(ctx: &TeamRenderContext<'_>, color: Color) -> Style {
    let style = Style::default().fg(color);
    if ctx.config.chrome.roles.bold_states {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// The row background: focus, else a faint flash or recency highlight.
pub(super) fn row_background(
    ctx: &TeamRenderContext<'_>,
    focused: bool,
    phase: AgentRowAnimationPhase,
) -> Option<Color> {
    let palette = &ctx.config.palette;
    if focused {
        Some(ctx.config.chrome.roles.row_focus_bg)
    } else if phase.status_flash > 0.0 || phase.recency_highlight > 0.0 {
        Some(blend_color(
            palette.sidebar_bg,
            palette.surface0,
            phase.status_flash.max(phase.recency_highlight * 0.65),
            palette,
        ))
    } else {
        None
    }
}

fn digits(value: u16) -> u16 {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1000..=9999 => 4,
        _ => 5,
    }
}

/// Retain the most urgent chips that fit; low-priority counts yield first.
pub(super) fn counts_width_fitting(
    counts: &StateCounts,
    available: u16,
    ctx: &TeamRenderContext<'_>,
) -> u16 {
    let mut width = 0;
    for state in ROLLUP {
        let count = counts.get(state);
        if count == 0 {
            continue;
        }
        let next = display_width(ctx.config.chrome.glyphs.state(state)) + digits(count) + 1;
        if width + next > available {
            break;
        }
        width += next;
    }
    width
}

/// The right-aligned roll-up of `counts` (`◐1 ▲1`), ending one cell before the
/// right edge; returns the column it starts at (`rect.right()` when empty).
pub(super) fn put_counts(
    buffer: &mut Buffer,
    rect: Rect,
    left_limit: u16,
    counts: &StateCounts,
    ctx: &TeamRenderContext<'_>,
) -> u16 {
    let chrome = &ctx.config.chrome;
    let right = rect.right();
    let width = counts_width_fitting(
        counts,
        right.saturating_sub(left_limit.saturating_add(1)),
        ctx,
    );
    let start = right.saturating_sub(width);
    if width == 0 {
        return right;
    }
    let mut x = start;
    for state in ROLLUP {
        let count = counts.get(state);
        if count == 0 {
            continue;
        }
        let chip = display_width(chrome.glyphs.state(state)) + digits(count) + 1;
        if x + chip > right {
            break;
        }
        let style = state_style(ctx, chrome.roles.state(state));
        x = put_segment(buffer, x, rect.y, right, chrome.glyphs.state(state), style);
        x = put_number(buffer, x, rect.y, right, usize::from(count), style);
        x = x.saturating_add(1);
    }
    start
}

/// A state's glyph and color for surfaces that do not draw through
/// `TeamRenderContext` (the flat agent list, the collapsed sidebar, mobile):
/// the same glyph set and roles, from the same approval-aware `StateKind`.
pub(in crate::client::shell) fn state_glyph_for(
    config: &crate::client::shell::ClientShellConfig,
    state: StateKind,
    phase: AgentRowAnimationPhase,
) -> (&'static str, Color) {
    let chrome = &config.chrome;
    let palette = &config.palette;
    let glyph = if state == StateKind::Working {
        chrome.glyphs.working_frame(phase.spinner_index)
    } else {
        chrome.glyphs.state(state)
    };
    let mut color = chrome.roles.state(state);
    if matches!(state, StateKind::Blocked | StateKind::Approval) && phase.blocked_pulse > 0.0 {
        color = blend_color(color, palette.surface1, phase.blocked_pulse * 0.65, palette);
    }
    if phase.status_flash > 0.0 {
        color = blend_color(color, palette.text, phase.status_flash * 0.75, palette);
    }
    (glyph, color)
}

/// `state_glyph_for` for a surface that only knows an `AgentStatus`.
pub(in crate::client::shell) fn agent_glyph(
    config: &crate::client::shell::ClientShellConfig,
    status: AgentStatus,
    phase: AgentRowAnimationPhase,
) -> (&'static str, Color) {
    state_glyph_for(config, StateKind::from_status(status), phase)
}
