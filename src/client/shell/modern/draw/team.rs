//! The lead row, role cards and member rows of the modern team tree.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{counts_width, put_counts, row_background, state_glyph};
use crate::client::shell::modern::model::rows::Tier;
use crate::client::shell::team_rows::TeamLineRef;
use crate::client::shell::team_sidebar::{TeamRenderContext, TeamRow, TeamSidebar};
use crate::client::shell::ShellHitMap;
use crate::ui::chrome::draw::{
    display_width, put_elided, put_number, put_segment_inline as put_segment,
};

const LEAD_INDENT: u16 = 1;
const GUTTER_X: u16 = 1;
const GLYPH_X: u16 = 3;
const NAME_X: u16 = 5;
const TITLE_X: u16 = 7;
const PINNED_LABEL: &str = " pinned";
const LEAD_LABEL: &str = "lead";
const OTHER_TITLE: &str = "other";
/// Cells a role name keeps before its roll-up may take the rest.
const MIN_NAME: u16 = 6;

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
        TeamLineRef::OtherHeader {
            count, collapsed, ..
        } => {
            other_header(buffer, rect, count, collapsed, ctx);
            hits.team_groups.push((rect, team.groups.len()));
        }
        _ => return super::lanes::render_line(buffer, rect, index, line, team, ctx, hits),
    }
    true
}

/// A recent-change marker (static; see `modern::model::recent`).
fn recent(row: &TeamRow, ctx: &TeamRenderContext<'_>) -> bool {
    row.pane_id
        .as_deref()
        .is_some_and(|pane_id| ctx.recent.is_recent(pane_id, ctx.now))
}

fn focused(row: &TeamRow, ctx: &TeamRenderContext<'_>) -> bool {
    row.pane_id
        .as_deref()
        .is_some_and(|pane_id| ctx.focused_pane_id == Some(pane_id))
}

fn lead(buffer: &mut Buffer, rect: Rect, team: &TeamSidebar, ctx: &TeamRenderContext<'_>) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
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
    // The label is decoration: the name gets whatever the label leaves, never less.
    let label_width = display_width(LEAD_LABEL);
    let name_right = if right.saturating_sub(x) > label_width + 8 {
        right - label_width - 2
    } else {
        right
    };
    let mut name = Style::default()
        .fg(roles.text_strong)
        .add_modifier(Modifier::BOLD);
    if recent(row, ctx) {
        name = name.add_modifier(Modifier::UNDERLINED);
    }
    x = put_elided(
        buffer,
        x,
        rect.y,
        name_right,
        &row.name,
        chrome.glyphs.ellipsis,
        name,
    );
    let start = right.saturating_sub(label_width + 1);
    if start > x {
        put_segment(
            buffer,
            start,
            rect.y,
            right,
            LEAD_LABEL,
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
    let muted = Style::default().fg(roles.text_muted);
    // Reserved first: the collapsed arrow and the state roll-up are the point
    // of the header; the name shrinks around them.
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
    let counts_width = counts_width(&card.counts, ctx);
    let counts_start = limit.saturating_sub(counts_width);
    let counts_rect = Rect::new(rect.x, rect.y, limit.saturating_sub(rect.x), 1);
    let mut x = put_segment(
        buffer,
        rect.x + GUTTER_X,
        rect.y,
        right,
        chrome.glyphs.role_gutter,
        tint,
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    let name_right = if counts_width > 0 && counts_start > x + MIN_NAME {
        counts_start - 1
    } else {
        limit
    };
    x = put_elided(
        buffer,
        x,
        rect.y,
        name_right,
        &entry.title,
        chrome.glyphs.ellipsis,
        Style::default()
            .fg(roles.text_body)
            .add_modifier(Modifier::BOLD),
    );
    let counts_start = put_counts(buffer, counts_rect, x, &card.counts, ctx);
    if entry.pinned && tier == Tier::Wide && counts_start > x + 2 + display_width(PINNED_LABEL) {
        x = put_segment(
            buffer,
            x,
            rect.y,
            counts_start.saturating_sub(1),
            PINNED_LABEL,
            muted,
        );
    }
    // Area hint: the role's oldest open task, when there is room after the name.
    if tier != Tier::Narrow && !card.hint.is_empty() {
        let hint_x = x + 2;
        let hint_right = counts_start.saturating_sub(1);
        if hint_right > hint_x + 4 {
            put_elided(
                buffer,
                hint_x,
                rect.y,
                hint_right,
                &card.hint,
                chrome.glyphs.ellipsis,
                Style::default().fg(roles.text_subtle),
            );
        }
    }
}

/// `kind`, or `kind` and `model` joined by the glyph set's separator.
fn kind_model_width(kind: &str, model: Option<&str>, separator: &str) -> u16 {
    display_width(kind).saturating_add(model.map_or(0, |model| {
        display_width(separator).saturating_add(display_width(model))
    }))
}

fn put_kind_model(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    (kind, model): (&str, Option<&str>),
    separator: &str,
    style: Style,
) {
    let mut x = put_segment(buffer, x, y, right, kind, style);
    if let Some(model) = model {
        x = put_segment(buffer, x, y, right, separator, style);
        put_segment(buffer, x, y, right, model, style);
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
        .add_modifier(dim)
        .add_modifier(if recent(row, ctx) {
            Modifier::UNDERLINED
        } else {
            Modifier::empty()
        });
    let meta = Style::default().fg(roles.text_muted).add_modifier(dim);
    let separator = chrome.glyphs.separator;
    // Right side, best first: kind and model, kind, nothing; a member without a
    // live agent shows its supervision phase. It is decoration, so the name is
    // drawn first at its full width and the meta only takes what is left.
    let name_x = rect.x + NAME_X;
    let name_width = display_width(&row.name);
    let x = name_x + name_width.min(right.saturating_sub(name_x));
    let choices: [(&str, Option<&str>); 2] = if row.live {
        [(&extra.kind, extra.model.as_deref()), (&extra.kind, None)]
    } else {
        [(&extra.phase, None), ("", None)]
    };
    let mut name_right = right;
    for (kind, model) in choices {
        let width = kind_model_width(kind, model, separator);
        let start = right.saturating_sub(width + 1);
        if width > 0 && start > x + 1 {
            put_kind_model(buffer, start, rect.y, right, (kind, model), separator, meta);
            name_right = start.saturating_sub(1);
            break;
        }
    }
    put_elided(
        buffer,
        name_x,
        rect.y,
        name_right.max(name_x),
        &row.name,
        chrome.glyphs.ellipsis,
        name_style,
    );
    if rect.height > 1 && tier == Tier::Wide {
        if let Some(title) = extra.task_title.as_deref() {
            put_elided(
                buffer,
                rect.x + TITLE_X,
                rect.y + 1,
                right,
                title,
                chrome.glyphs.ellipsis,
                Style::default().fg(roles.text_subtle),
            );
        }
    }
}

/// The `other` group header, in the active glyph set.
fn other_header(
    buffer: &mut Buffer,
    rect: Rect,
    count: usize,
    collapsed: bool,
    ctx: &TeamRenderContext<'_>,
) {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let right = rect.right();
    let glyph = if collapsed {
        chrome.glyphs.group_closed
    } else {
        chrome.glyphs.group_open
    };
    let muted = Style::default().fg(roles.text_muted);
    let mut x = put_segment(buffer, rect.x + LEAD_INDENT, rect.y, right, glyph, muted);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(
        buffer,
        x,
        rect.y,
        right,
        OTHER_TITLE,
        Style::default()
            .fg(roles.text_body)
            .add_modifier(Modifier::BOLD),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    put_number(buffer, x, rect.y, right, count, muted);
}
