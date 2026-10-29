//! The work board of the modern sidebar: lane headers, task cards with age
//! bars and relay or parallel progress, and the done lane.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::{row_background, state_glyph, state_style};
use crate::client::shell::modern::model::lanes::{StepNode, StepStrip, StripKind, TaskCard};
use crate::client::shell::modern::model::rows::Tier;
use crate::client::shell::team_rows::TeamLineRef;
use crate::client::shell::team_sidebar::{TeamRenderContext, TeamSidebar};
use crate::client::shell::{ChromeTarget, ShellHitMap};
use crate::ui::chrome::age::{age_bar_cells, age_text, age_tone, AgeTone, AGE_BUFFER_LEN};
use crate::ui::chrome::draw::{display_width, put_bar, put_elided, put_number, put_segment};
use crate::ui::chrome::state::{Lane, StateKind};

const LABEL_INDENT: u16 = 1;
const CARD_INDENT: u16 = 3;
const DETAIL_INDENT: u16 = 5;
const WORK_LABEL: &str = "WORK";
const AGE_BAR_CELLS: u8 = 3;
/// Resolved tasks dim from this age on, a calm hint that they will leave.
const DONE_DIM_AFTER_MS: u64 = 5 * 60 * 1000;

pub(in crate::client::shell) fn render_line(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    line: &TeamLineRef,
    team: &TeamSidebar,
    ctx: &TeamRenderContext<'_>,
    hits: &mut ShellHitMap,
) -> bool {
    let tier = team.rows.tier.unwrap_or(Tier::Normal);
    match *line {
        TeamLineRef::WorkLabel => {
            work_label(buffer, rect, team, ctx);
            // The label opens the flow view (a no-op when `ui.flow_view` is off).
            if ctx.config.flow_view {
                hits.chrome_targets.push((rect, ChromeTarget::FlowOpen));
            }
        }
        TeamLineRef::LaneHeader {
            lane,
            collapsed,
            hidden,
        } => {
            lane_header(buffer, rect, lane, collapsed, hidden, team, ctx);
            hits.chrome_targets.push((rect, ChromeTarget::Lane(lane)));
        }
        TeamLineRef::LaneTask { lane, index: card } => {
            let card = &team.modern.lanes.tasks(lane)[card];
            task_card(buffer, rect, lane, card, tier, ctx);
            if card.pane_id.is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        TeamLineRef::LaneDone(card) => done_row(buffer, rect, team, card, ctx),
        TeamLineRef::Gap => {}
        _ => return false,
    }
    true
}

/// `WORK` and a right-aligned count per non-empty lane.
fn work_label(buffer: &mut Buffer, rect: Rect, team: &TeamSidebar, ctx: &TeamRenderContext<'_>) {
    let chrome = &ctx.config.chrome;
    let right = rect.right();
    let label = Style::default()
        .fg(chrome.roles.text_muted)
        .add_modifier(Modifier::BOLD);
    let x = put_segment(
        buffer,
        rect.x + LABEL_INDENT,
        rect.y,
        right,
        WORK_LABEL,
        label,
    );
    let mut width = 0u16;
    for lane in Lane::ALL {
        let count = lane_count(team, lane);
        if count > 0 {
            width += 1 + digit_width(count) + 1;
        }
    }
    let start = right.saturating_sub(width);
    if width == 0 || start <= x {
        return;
    }
    let mut x = start;
    for lane in Lane::ALL {
        let count = lane_count(team, lane);
        if count == 0 {
            continue;
        }
        let style = state_style(ctx, lane_color(lane, ctx));
        x = put_segment(buffer, x, rect.y, right, lane_glyph(lane, ctx), style);
        x = put_number(buffer, x, rect.y, right, count, style);
        x = x.saturating_add(1);
    }
}

fn lane_count(team: &TeamSidebar, lane: Lane) -> usize {
    if lane == Lane::Done {
        team.rows.visible_done
    } else {
        team.modern.lanes.len(lane)
    }
}

fn digit_width(count: usize) -> u16 {
    match count {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        _ => 4,
    }
}

fn lane_glyph(lane: Lane, ctx: &TeamRenderContext<'_>) -> &'static str {
    let glyphs = ctx.config.chrome.glyphs;
    match lane {
        Lane::NeedsYou => glyphs.blocked,
        other => glyphs.state(other.state()),
    }
}

fn lane_color(lane: Lane, ctx: &TeamRenderContext<'_>) -> Color {
    let roles = &ctx.config.chrome.roles;
    match lane {
        Lane::NeedsYou => roles.state_approval,
        other => roles.state(other.state()),
    }
}

fn lane_header(
    buffer: &mut Buffer,
    rect: Rect,
    lane: Lane,
    collapsed: bool,
    hidden: u16,
    team: &TeamSidebar,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let right = rect.right();
    let muted = Style::default().fg(chrome.roles.text_muted);
    let mut x = put_segment(
        buffer,
        rect.x + LABEL_INDENT,
        rect.y,
        right,
        lane_glyph(lane, ctx),
        state_style(ctx, lane_color(lane, ctx)),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(
        buffer,
        x,
        rect.y,
        right,
        lane.label(),
        Style::default()
            .fg(chrome.roles.text_body)
            .add_modifier(Modifier::BOLD),
    );
    x = put_segment(buffer, x, rect.y, right, "  ", Style::default());
    x = put_number(buffer, x, rect.y, right, lane_count(team, lane), muted);
    let mut limit = right.saturating_sub(1);
    if collapsed {
        limit = limit.saturating_sub(1);
        put_segment(
            buffer,
            limit,
            rect.y,
            right,
            chrome.glyphs.group_closed,
            muted,
        );
    }
    if hidden > 0 {
        // `+N` for resolved tasks that did not fit, right before the arrow.
        let width = 1 + digit_width(usize::from(hidden));
        let start = limit.saturating_sub(width + 1);
        if start > x {
            let after = put_segment(buffer, start, rect.y, right, "+", muted);
            put_number(buffer, after, rect.y, right, usize::from(hidden), muted);
        }
    }
}

fn node_glyph(node: StepNode, ctx: &TeamRenderContext<'_>) -> (&'static str, Color) {
    let chrome = &ctx.config.chrome;
    match node {
        StepNode::Done => (chrome.glyphs.node_done, chrome.roles.state_done),
        StepNode::Active => (chrome.glyphs.node_active, chrome.roles.state_working),
        StepNode::Pending => (chrome.glyphs.node_pending, chrome.roles.state_idle),
        StepNode::Failed => (chrome.glyphs.node_failed, chrome.roles.state_blocked),
        StepNode::Omitted => (chrome.glyphs.ellipsis, chrome.roles.text_muted),
    }
}

fn age_color(lane: Lane, elapsed_ms: u64, ctx: &TeamRenderContext<'_>) -> Color {
    let roles = &ctx.config.chrome.roles;
    if lane == Lane::NeedsYou {
        return roles.state_approval;
    }
    match age_tone(elapsed_ms) {
        AgeTone::Fresh => roles.text_muted,
        AgeTone::Slow => roles.state_approval,
        AgeTone::Stale => roles.state_blocked,
    }
}

fn task_card(
    buffer: &mut Buffer,
    rect: Rect,
    lane: Lane,
    card: &TaskCard,
    tier: Tier,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let right = rect.right();
    let is_focused = card
        .pane_id
        .as_deref()
        .is_some_and(|pane_id| ctx.focused_pane_id == Some(pane_id));
    let (glyph, glyph_style, phase) =
        state_glyph(ctx, card.state, card.pane_id.as_deref().unwrap_or_default());
    if let Some(bg) = row_background(ctx, is_focused, phase) {
        buffer.set_style(rect, Style::default().bg(bg));
    }
    let mut who = Style::default().fg(if lane == Lane::NeedsYou || is_focused {
        roles.text_strong
    } else {
        roles.text_body
    });
    if card
        .pane_id
        .as_deref()
        .is_some_and(|pane_id| ctx.recent.is_recent(pane_id, ctx.now))
    {
        who = who.add_modifier(Modifier::UNDERLINED);
    }
    // Reserved first: the age. Then who, elided to what the age leaves. The
    // bar and the short id are decoration and only use space left over.
    let elapsed = ctx.now_unix_ms.saturating_sub(card.created_at_ms);
    let mut buffer_age = [0u8; AGE_BUFFER_LEN];
    let age = age_text(elapsed, &mut buffer_age);
    let age_width = display_width(age);
    let age_x = right.saturating_sub(age_width + 1);
    let who_right = age_x.saturating_sub(1);
    let mut x = put_segment(
        buffer,
        rect.x + CARD_INDENT,
        rect.y,
        right,
        glyph,
        glyph_style,
    );
    x = put_segment(buffer, x, rect.y, who_right, " ", Style::default());
    x = put_elided(
        buffer,
        x,
        rect.y,
        who_right,
        &card.who,
        chrome.glyphs.ellipsis,
        who,
    );
    if tier == Tier::Wide {
        let id_width = display_width(&card.short_id);
        if x + 1 + id_width <= who_right {
            x = put_segment(buffer, x, rect.y, who_right, " ", Style::default());
            x = put_segment(
                buffer,
                x,
                rect.y,
                who_right,
                &card.short_id,
                Style::default().fg(roles.text_muted),
            );
        }
    }
    let bar_width = u16::from(AGE_BAR_CELLS) + 1;
    if tier != Tier::Narrow && age_x > x + bar_width {
        put_bar(
            buffer,
            age_x - bar_width,
            rect.y,
            age_x,
            (age_bar_cells(elapsed), AGE_BAR_CELLS),
            (
                (
                    chrome.glyphs.bar_full,
                    Style::default().fg(age_color(lane, elapsed, ctx)),
                ),
                (chrome.glyphs.bar_empty, Style::default().fg(roles.rule)),
            ),
        );
    }
    if age_x > rect.x + CARD_INDENT {
        put_segment(
            buffer,
            age_x,
            rect.y,
            right,
            age,
            Style::default().fg(roles.text_muted),
        );
    }
    if rect.height > 1 {
        put_elided(
            buffer,
            rect.x + DETAIL_INDENT,
            rect.y + 1,
            right,
            &card.title,
            chrome.glyphs.ellipsis,
            Style::default().fg(roles.text_subtle),
        );
    }
    if rect.height > 2 {
        if let Some(strip) = card.strip.as_ref() {
            strip_row(
                buffer,
                rect.x + DETAIL_INDENT,
                rect.y + 2,
                right,
                strip,
                tier,
                ctx,
            );
        }
    }
}

/// `●━◐━○  step 2/3 ▶ codex`, `‖ parallel 1/2`, or `⊕ integrating`.
///
/// The progress text is essential and is laid out first; the node glyphs are
/// decoration and shrink (drop their connectors, then show a window around the
/// active node, then vanish) to whatever width is left.
fn strip_row(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    strip: &StepStrip,
    tier: Tier,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let muted = Style::default().fg(roles.text_muted);
    let available = right.saturating_sub(x);
    let line_width = display_width(&strip.line);
    match strip.kind {
        StripKind::Relay => {
            if line_width > available {
                put_elided(
                    buffer,
                    x,
                    y,
                    right,
                    &strip.line,
                    chrome.glyphs.ellipsis,
                    muted,
                );
                return;
            }
            let nodes = strip.nodes.len() as u16;
            let spare = available - line_width;
            let gap = if spare > 2 { 2 } else { 0 };
            let room = spare - gap;
            let connectors = room >= nodes.saturating_mul(2).saturating_sub(1);
            let shown = if connectors { nodes } else { room.min(nodes) };
            let active = strip
                .nodes
                .iter()
                .position(|node| *node == StepNode::Active)
                .unwrap_or(strip.nodes.len().saturating_sub(1));
            let first = active
                .saturating_sub(usize::from(shown) / 2)
                .min(strip.nodes.len() - usize::from(shown));
            let mut x = x;
            for (position, node) in strip.nodes[first..first + usize::from(shown)]
                .iter()
                .enumerate()
            {
                if connectors && position > 0 {
                    x = put_segment(
                        buffer,
                        x,
                        y,
                        right,
                        chrome.glyphs.bar_full,
                        Style::default().fg(roles.flow),
                    );
                }
                let (glyph, color) = node_glyph(*node, ctx);
                x = put_segment(buffer, x, y, right, glyph, state_style(ctx, color));
            }
            if shown > 0 && gap > 0 {
                x = put_segment(buffer, x, y, right, "  ", Style::default());
            }
            x = put_segment(buffer, x, y, right, &strip.line, muted);
            if tier == Tier::Wide {
                if let Some(next) = strip.next.as_deref() {
                    // ` ▶ next`, only when all of it fits.
                    if right.saturating_sub(x) >= display_width(next) + 3 {
                        x = put_segment(buffer, x, y, right, " ", Style::default());
                        x = put_segment(
                            buffer,
                            x,
                            y,
                            right,
                            chrome.glyphs.handoff,
                            Style::default().fg(roles.flow),
                        );
                        x = put_segment(buffer, x, y, right, " ", Style::default());
                        put_segment(buffer, x, y, right, next, muted);
                    }
                }
            }
        }
        StripKind::Parallel | StripKind::Integrating => {
            let glyph = if strip.kind == StripKind::Parallel {
                chrome.glyphs.parallel
            } else {
                chrome.glyphs.integrate
            };
            let mut x = put_segment(buffer, x, y, right, glyph, state_style(ctx, roles.parallel));
            x = put_segment(buffer, x, y, right, " ", Style::default());
            x = put_elided(
                buffer,
                x,
                y,
                right,
                &strip.line,
                chrome.glyphs.ellipsis,
                muted,
            );
            if tier == Tier::Wide {
                for node in &strip.nodes {
                    // One branch glyph per node, while a whole one still fits.
                    if x + 2 > right {
                        break;
                    }
                    x = put_segment(buffer, x, y, right, " ", Style::default());
                    let (glyph, color) = node_glyph(*node, ctx);
                    x = put_segment(buffer, x, y, right, glyph, state_style(ctx, color));
                }
            }
        }
    }
}
fn done_row(
    buffer: &mut Buffer,
    rect: Rect,
    team: &TeamSidebar,
    card: usize,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let card = &team.modern.lanes.done[card];
    let right = rect.right();
    let elapsed = ctx.now_unix_ms.saturating_sub(card.finished_at_ms);
    let dim = if elapsed >= DONE_DIM_AFTER_MS {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let glyph_color = match card.state {
        StateKind::Done => roles.state_done,
        StateKind::Failed => roles.state_blocked,
        _ => roles.state_idle,
    };
    let mut x = put_segment(
        buffer,
        rect.x + CARD_INDENT,
        rect.y,
        right,
        chrome.glyphs.state(card.state),
        state_style(ctx, glyph_color).add_modifier(dim),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    let mut buffer_age = [0u8; AGE_BUFFER_LEN];
    let age = age_text(elapsed, &mut buffer_age);
    let age_width = display_width(age);
    let summary_right = right.saturating_sub(age_width + 2).max(x);
    put_elided(
        buffer,
        x,
        rect.y,
        summary_right,
        &card.summary,
        chrome.glyphs.ellipsis,
        Style::default().fg(roles.text_subtle).add_modifier(dim),
    );
    put_segment(
        buffer,
        right.saturating_sub(age_width + 1),
        rect.y,
        right,
        age,
        Style::default().fg(roles.text_muted).add_modifier(dim),
    );
}
