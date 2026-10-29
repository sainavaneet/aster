//! The lead row, role cards and member rows of the modern team tree.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{put_counts, row_background, state_glyph};
use crate::client::shell::modern::model::rows::Tier;
use crate::client::shell::team_rows::TeamLineRef;
use crate::client::shell::team_sidebar::{TeamRenderContext, TeamRow, TeamSidebar};
use crate::client::shell::ShellHitMap;
use crate::ui::chrome::draw::{display_width, put_segment};

const LEAD_INDENT: u16 = 1;
const GUTTER_X: u16 = 1;
const GLYPH_X: u16 = 3;
const NAME_X: u16 = 5;
const TITLE_X: u16 = 7;
const PINNED_LABEL: &str = " pinned";

/// Draws a modern team row; `false` for rows the classic drawer owns (`other`).
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
        TeamLineRef::Lead => {
            lead(buffer, rect, team, ctx);
            if team.lead.pane_id.is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        TeamLineRef::Group { group, collapsed } => {
            role_header(buffer, rect, group, collapsed, tier, team, ctx);
            hits.team_groups.push((rect, group));
        }
        TeamLineRef::Member { group, member } => {
            member_row(buffer, rect, group, member, tier, team, ctx);
            if team.groups[group].members[member].pane_id.is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        _ => return super::lanes::render_line(buffer, rect, index, line, team, ctx, hits),
    }
    true
}

fn focused(row: &TeamRow, ctx: &TeamRenderContext<'_>) -> bool {
    row.pane_id
        .as_deref()
        .is_some_and(|pane_id| ctx.focused_pane_id == Some(pane_id))
}

fn lead(buffer: &mut Buffer, rect: Rect, team: &TeamSidebar, ctx: &TeamRenderContext<'_>) {
    let roles = &ctx.config.chrome.roles;
    let row = &team.lead;
    let state = team.modern.lead.state;
    let (glyph, glyph_style, phase) =
        state_glyph(ctx, state, row.pane_id.as_deref().unwrap_or_default());
    if let Some(bg) = row_background(ctx, focused(row, ctx), phase) {
        buffer.set_style(rect, Style::default().bg(bg));
    }
    let right = rect.right();
    let mut x = put_segment(
        buffer,
        rect.x + LEAD_INDENT,
        rect.y,
        right,
        glyph,
        glyph_style,
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    let name = Style::default()
        .fg(roles.text_strong)
        .add_modifier(Modifier::BOLD);
    x = put_segment(buffer, x, rect.y, right, &row.name, name);
    let label = &team.modern.lead.kind_model;
    let label_width = display_width(label);
    let start = right.saturating_sub(label_width + 1);
    if start > x {
        put_segment(
            buffer,
            start,
            rect.y,
            right,
            label,
            Style::default().fg(roles.text_muted),
        );
    }
}

fn role_header(
    buffer: &mut Buffer,
    rect: Rect,
    group: usize,
    collapsed: bool,
    tier: Tier,
    team: &TeamSidebar,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let entry = &team.groups[group];
    let card = &team.modern.roles[group];
    let right = rect.right();
    let tint = Style::default().fg(roles.tint(group));
    let mut x = put_segment(
        buffer,
        rect.x + GUTTER_X,
        rect.y,
        right,
        chrome.glyphs.role_gutter,
        tint,
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(
        buffer,
        x,
        rect.y,
        right,
        &entry.title,
        Style::default()
            .fg(roles.text_body)
            .add_modifier(Modifier::BOLD),
    );
    let muted = Style::default().fg(roles.text_muted);
    if entry.pinned && tier == Tier::Wide {
        x = put_segment(buffer, x, rect.y, right, PINNED_LABEL, muted);
    }
    // A collapsed card keeps its roll-up and says it is collapsed.
    let mut limit = right;
    if collapsed {
        limit = right.saturating_sub(2);
        put_segment(
            buffer,
            limit,
            rect.y,
            right,
            chrome.glyphs.group_closed,
            muted,
        );
    }
    let counts_rect = Rect::new(rect.x, rect.y, limit.saturating_sub(rect.x), 1);
    let counts_start = put_counts(buffer, counts_rect, x + 1, &card.counts, ctx);
    // Area hint: the role's oldest open task, when there is room after the name.
    if tier != Tier::Narrow && !card.hint.is_empty() {
        let hint_x = x + 2;
        let hint_right = counts_start.saturating_sub(1);
        if hint_right > hint_x + 4 {
            put_segment(
                buffer,
                hint_x,
                rect.y,
                hint_right,
                &card.hint,
                Style::default().fg(roles.text_subtle),
            );
        }
    }
}

fn member_row(
    buffer: &mut Buffer,
    rect: Rect,
    group: usize,
    member: usize,
    tier: Tier,
    team: &TeamSidebar,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let row = &team.groups[group].members[member];
    let extra = &team.modern.roles[group].members[member];
    let is_focused = focused(row, ctx);
    let (glyph, glyph_style, phase) =
        state_glyph(ctx, extra.state, row.pane_id.as_deref().unwrap_or_default());
    if let Some(bg) = row_background(ctx, is_focused, phase) {
        buffer.set_style(rect, Style::default().bg(bg));
    }
    let right = rect.right();
    let dim = if row.live {
        Modifier::empty()
    } else {
        Modifier::DIM
    };
    put_segment(
        buffer,
        rect.x + GUTTER_X,
        rect.y,
        right,
        chrome.glyphs.role_gutter,
        Style::default().fg(roles.tint(group)),
    );
    put_segment(
        buffer,
        rect.x + GLYPH_X,
        rect.y,
        right,
        glyph,
        glyph_style.add_modifier(dim),
    );
    let name_style = Style::default()
        .fg(if is_focused {
            roles.text_strong
        } else {
            roles.text_body
        })
        .add_modifier(if is_focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        })
        .add_modifier(dim);
    let x = put_segment(
        buffer,
        rect.x + NAME_X,
        rect.y,
        right,
        &row.name,
        name_style,
    );
    // Right side: kind·model, else kind, else nothing; a member without a live
    // agent shows its supervision phase instead.
    let meta = Style::default().fg(roles.text_muted).add_modifier(dim);
    let candidates: [&str; 2] = if row.live {
        [&extra.kind_model, &extra.kind]
    } else {
        [&extra.phase, ""]
    };
    for text in candidates {
        let width = display_width(text);
        let start = right.saturating_sub(width + 1);
        if width > 0 && start > x {
            put_segment(buffer, start, rect.y, right, text, meta);
            break;
        }
    }
    if rect.height > 1 && tier == Tier::Wide {
        if let Some(title) = extra.task_title.as_deref() {
            put_segment(
                buffer,
                rect.x + TITLE_X,
                rect.y + 1,
                right,
                title,
                Style::default().fg(roles.text_subtle),
            );
        }
    }
}
