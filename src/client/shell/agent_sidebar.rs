use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};

use super::*;

pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) status: crate::api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
}

pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    if snapshot.agent_view_label.is_some() {
        return snapshot
            .agent_order
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane_id.as_str())
            })
            .cloned()
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == crate::config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    endpoint_id: &ClientEndpointId,
    animation: super::animation::AnimationFrame<'_>,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !render_agent_panel_header(
        buffer,
        area,
        AGENT_PANEL_TITLE,
        snapshot.agent_view_label.as_deref(),
        config,
        hits,
    ) {
        return;
    }

    let rows = agent_rows(snapshot, config, None);
    render_agent_list(
        buffer,
        area,
        &rows,
        snapshot
            .agent_view_label
            .as_ref()
            .map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        config.agents.row_gap,
        |row| row.rows.len(),
        |buffer, rect, row, hits| {
            let phase = animation.agent_row(endpoint_id, &row.pane_id, row.status);
            hits.agents.push((rect, row.pane_id.clone()));
            render_agent_row(buffer, rect, row, config, phase);
        },
    );
}

pub(super) const AGENT_PANEL_TITLE: &str = " agents";

pub(super) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    agent_view_label: Option<&str>,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(config.palette.surface_dim),
    );
    if area.height < 2 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        title,
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => "grouped",
        crate::config::AgentPanelSortConfig::Priority => "priority",
    });
    let sort_width = display_width(sort_label).min(area.width as usize) as u16;
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    hits.agent_sort_toggle = if config.mouse_capture && agent_view_label.is_none() {
        sort_rect
    } else {
        Rect::default()
    };
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(if agent_view_label.is_some() {
                config.palette.accent
            } else {
                config.palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    true
}

pub(super) fn render_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[T],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    row_gap: u16,
    row_lines: impl Fn(&T) -> usize,
    mut render_row: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let row_heights = rows
        .iter()
        .map(|row| row_height(row_lines(row)))
        .collect::<Vec<_>>();
    let gaps = list_gaps(rows.len(), row_gap);
    render_measured_agent_list(
        buffer,
        area,
        AgentListRows {
            rows,
            heights: &row_heights,
            gaps: &gaps,
        },
        empty_message,
        config,
        agent_scroll,
        hits,
        |buffer, rect, _, row, hits| render_row(buffer, rect, row, hits),
    );
}

/// A list row's height in cells from its line count.
pub(super) fn row_height(lines: usize) -> u16 {
    lines.max(1).min(u16::MAX as usize) as u16
}

/// `row_gap` after every row but the last.
pub(super) fn list_gaps(rows: usize, row_gap: u16) -> Vec<u16> {
    (0..rows)
        .map(|index| if index + 1 < rows { row_gap } else { 0 })
        .collect()
}

/// Rows with their heights and the gap after each, all the same length.
pub(super) struct AgentListRows<'a, T> {
    pub(super) rows: &'a [T],
    pub(super) heights: &'a [u16],
    pub(super) gaps: &'a [u16],
}

/// `render_agent_list` for rows measured ahead of time, so a caller that
/// caches its rows allocates nothing here. `render_row` gets the row index.
pub(super) fn render_measured_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    list: AgentListRows<'_, T>,
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    mut render_row: impl FnMut(&mut Buffer, Rect, usize, &T, &mut ShellHitMap),
) {
    let AgentListRows {
        rows,
        heights: row_heights,
        gaps,
    } = list;
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || rows.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let metrics = super::scroll::list_scroll_metrics(row_heights, gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*agent_scroll) {
        let height = row_heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        render_row(buffer, rect, index, row, hits);
        y = y.saturating_add(height).saturating_add(gaps[index]);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
        .into_iter()
        .filter_map(|pane_id| agent_row(snapshot, &pane_id, config, machine))
        .collect()
}

pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<AgentRow> {
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
    let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == agent.pane_id);
    let tab_count = snapshot
        .tabs
        .iter()
        .filter(|candidate| candidate.workspace_id == agent.workspace_id)
        .count();
    let tab_label = tab
        .filter(|tab| tab_count > 1 || tab.custom_label)
        .map(|tab| tab.label.as_str());
    let agent_label = agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref());
    let labels = agent
        .state_labels
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let tokens = agent.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let state_text = labels
        .get(status_text(agent.agent_status))
        .map(String::as_str)
        .unwrap_or_else(|| sidebar_status_text(agent.agent_status));
    let canonical_agent = agent
        .agent
        .as_deref()
        .and_then(crate::detect::parse_agent_label);
    let rows = crate::ui::sidebar_agent_rows(
        &config.agents,
        crate::ui::AgentTokenContext {
            machine,
            workspace: &workspace.label,
            tab: tab_label,
            pane: agent
                .title
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.label.as_deref())),
            agent_label,
            terminal_title: agent.terminal_title.as_deref(),
            terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
            canonical_agent,
            tokens: &tokens,
        },
        state_text,
    );
    Some(AgentRow {
        pane_id: agent.pane_id.clone(),
        status: agent.agent_status,
        focused: agent.focused,
        rows,
    })
}

pub(super) fn agent_status_icon_and_color(
    status: crate::api::schema::AgentStatus,
    indicator_style: crate::config::StatusIndicatorStyle,
    palette: &Palette,
    phase: super::animation::AgentRowAnimationPhase,
) -> (&'static str, ratatui::style::Color) {
    let base_color = status_color(status, palette);
    let icon = if status == crate::api::schema::AgentStatus::Working {
        super::animation::working_spinner_glyph(phase.spinner_index, indicator_style)
    } else {
        status_icon(status, indicator_style)
    };
    let mut color =
        if status == crate::api::schema::AgentStatus::Blocked && phase.blocked_pulse > 0.0 {
            super::animation::blend_color(
                base_color,
                palette.surface1,
                phase.blocked_pulse * 0.65,
                palette,
            )
        } else {
            base_color
        };
    if phase.status_flash > 0.0 {
        color =
            super::animation::blend_color(color, palette.text, phase.status_flash * 0.75, palette);
    }
    (icon, color)
}

pub(super) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    config: &ClientShellConfig,
    phase: super::animation::AgentRowAnimationPhase,
) {
    let palette = &config.palette;
    let base_bg = if row.focused {
        palette.active_row_bg
    } else {
        palette.sidebar_bg
    };
    let row_bg = if phase.status_flash > 0.0 {
        Some(super::animation::blend_color(
            base_bg,
            palette.surface1,
            (phase.status_flash * 0.85).max(phase.recency_highlight * 0.55),
            palette,
        ))
    } else if phase.recency_highlight > 0.0 {
        Some(super::animation::blend_color(
            base_bg,
            palette.surface0,
            phase.recency_highlight * 0.65,
            palette,
        ))
    } else if row.focused {
        Some(palette.active_row_bg)
    } else {
        None
    };
    let row_style = row_bg.map_or_else(Style::default, |bg| Style::default().bg(bg));
    let name_fg = if row.focused {
        palette.text
    } else if phase.status_flash > 0.0 || phase.recency_highlight > 0.0 {
        super::animation::blend_color(
            palette.subtext0,
            palette.text,
            phase.status_flash.max(phase.recency_highlight * 0.75),
            palette,
        )
    } else {
        palette.subtext0
    };
    let name_style = Style::default().fg(name_fg).add_modifier(Modifier::BOLD);
    let (icon_glyph, status_fg) =
        agent_status_icon_and_color(row.status, config.status_indicators, palette, phase);
    let status_style = if phase.status_flash > 0.25 {
        Style::default().fg(status_fg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(status_fg)
    };
    let secondary = Style::default().fg(palette.overlay0);
    let icon = (icon_glyph, status_style);
    let rows = if row.rows.is_empty() {
        vec![vec![crate::ui::ResolvedToken {
            kind: crate::ui::ResolvedTokenKind::StateIcon,
            style: Default::default(),
        }]]
    } else {
        row.rows.clone()
    };
    for (index, tokens) in rows.iter().take(rect.height as usize).enumerate() {
        let indent = if index == 0 { 1 } else { 3 };
        let mut spans = vec![ratatui::text::Span::raw(" ".repeat(indent))];
        spans.extend(crate::ui::resolved_token_spans(
            tokens,
            icon,
            status_style,
            name_style,
            secondary,
            secondary,
            palette,
            rect.width.saturating_sub(indent as u16) as usize,
        ));
        Paragraph::new(Line::from(spans)).style(row_style).render(
            Rect::new(rect.x, rect.y + index as u16, rect.width, 1),
            buffer,
        );
    }
    if rect.width > 0 && (phase.status_flash > 0.0 || phase.recency_highlight > 0.0) {
        let bar_color = if phase.status_flash > 0.0 {
            super::animation::blend_color(
                status_color(row.status, palette),
                palette.text,
                phase.status_flash * 0.6,
                palette,
            )
        } else {
            super::animation::blend_color(
                palette.surface_dim,
                palette.accent,
                phase.recency_highlight,
                palette,
            )
        };
        let bar_glyph = if phase.status_flash > 0.35 || phase.recency_highlight > 0.45 {
            "▎"
        } else {
            "▏"
        };
        for y in rect.y..rect.bottom() {
            if let Some(cell) = buffer.cell_mut((rect.x, y)) {
                cell.set_symbol(bar_glyph).set_fg(bar_color);
            }
        }
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    for (offset, character) in text.chars().take(width as usize).enumerate() {
        if let Some(cell) = buffer.cell_mut((x + offset as u16, y)) {
            cell.set_char(character).set_style(style);
        }
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn sidebar_status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Working => "working",
        AgentStatus::Idle | AgentStatus::Unknown => "idle",
    }
}
