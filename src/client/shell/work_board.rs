//! Work board below the Orbit team tree: active assignments, then recently
//! resolved ones.
//!
//! `WorkBoard` is built together with `TeamSidebar` when the snapshot's `orbit`
//! projection changes, so every string a row shows is formatted once there. The
//! render loop only borrows that text; the two time-dependent facts, the age
//! label and the ten-minute hide of resolved tasks, come from the frame's
//! wall-clock reading and are written through stack buffers.

use std::collections::HashMap;

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier, style::Style};

use super::render::{display_width, put_segment, put_text};
use super::team_sidebar::{agent_state, TeamRenderContext};
use super::*;
use crate::api::schema::AgentStatus;
use crate::protocol::OrbitShellView;

/// How long a resolved task stays on the board after it finished.
pub(super) const RECENT_VISIBLE_MS: u64 = 10 * 60 * 1000;
const MAX_RECENT: usize = 5;
const SHORT_ID_CHARS: usize = 8;
const WORK_TITLE: &str = "work";
const TASK_INDENT: u16 = 1;
const TASK_DETAIL_INDENT: u16 = 3;
const APPROVAL_MARKER: &str = "⚑ ";
const EXPANDED_GLYPH: &str = "▾";
const COLLAPSED_GLYPH: &str = "▸";
/// Longest age label: four digits and a unit.
const AGE_BUFFER_LEN: usize = 5;
const MAX_AGE_VALUE: u64 = 9999;

pub(super) struct WorkTask {
    short_id: String,
    who: String,
    title: String,
    state: String,
    status: AgentStatus,
    awaiting_approval: bool,
    created_at_ms: u64,
    pane_id: Option<String>,
}

pub(super) struct WorkRecent {
    short_id: String,
    outcome: String,
    summary: String,
    glyph: &'static str,
    finished_at_ms: u64,
}

pub(super) struct WorkBoard {
    tasks: Vec<WorkTask>,
    recent: Vec<WorkRecent>,
    blocked: usize,
}

fn short_id(task_id: &str) -> String {
    let skip = task_id.chars().count().saturating_sub(SHORT_ID_CHARS);
    task_id.chars().skip(skip).collect()
}

fn outcome_glyph(outcome: &str) -> &'static str {
    match outcome {
        "completed" => "✓",
        "failed" => "×",
        "blocked" => "!",
        _ => "·",
    }
}

impl WorkBoard {
    pub(super) fn build(orbit: &OrbitShellView) -> Self {
        let mut names = HashMap::<&str, &str>::new();
        let mut panes = HashMap::<&str, &str>::new();
        names.insert(orbit.lead.terminal_id.as_str(), orbit.lead.name.as_str());
        if let Some(pane_id) = orbit.lead.pane_id.as_deref() {
            panes.insert(orbit.lead.terminal_id.as_str(), pane_id);
        }
        let members = orbit
            .roles
            .iter()
            .flat_map(|role| &role.members)
            .chain(&orbit.scratch);
        for member in members {
            let Some(terminal_id) = member.terminal_id.as_deref() else {
                continue;
            };
            names.insert(terminal_id, member.name.as_str());
            if let Some(pane_id) = member.pane_id.as_deref() {
                panes.insert(terminal_id, pane_id);
            }
        }
        let mut tasks = orbit
            .active_tasks
            .iter()
            .map(|task| {
                let worker = task.worker_terminal_id.as_str();
                let state = agent_state(&task.state);
                WorkTask {
                    short_id: short_id(&task.task_id),
                    who: task
                        .role
                        .clone()
                        .or_else(|| names.get(worker).map(|name| (*name).to_owned()))
                        .unwrap_or_else(|| worker.to_owned()),
                    title: task.title.clone(),
                    state: task.state.clone(),
                    status: if task.blocked {
                        AgentStatus::Blocked
                    } else {
                        state.unwrap_or(AgentStatus::Unknown)
                    },
                    awaiting_approval: task.awaiting_approval,
                    created_at_ms: task.created_at_ms,
                    pane_id: panes.get(worker).map(|pane_id| (*pane_id).to_owned()),
                }
            })
            .collect::<Vec<_>>();
        // Stable: blocked tasks on top, the rest keep ledger order.
        tasks.sort_by_key(|task| task.status != AgentStatus::Blocked);
        let blocked = tasks
            .iter()
            .filter(|task| task.status == AgentStatus::Blocked)
            .count();
        let recent = orbit
            .recent_tasks
            .iter()
            .take(MAX_RECENT)
            .map(|task| WorkRecent {
                short_id: short_id(&task.task_id),
                outcome: task.outcome.clone(),
                summary: task.summary.clone(),
                glyph: outcome_glyph(&task.outcome),
                finished_at_ms: task.finished_at_ms,
            })
            .collect();
        Self {
            tasks,
            recent,
            blocked,
        }
    }

    /// Appends the board's lines for a frame at `now_ms`, nothing when empty.
    /// Returns when the shown lines next change on their own: the first time a
    /// visible resolved task reaches the end of its window (`u64::MAX` if none).
    pub(super) fn push_line_refs(
        &self,
        lines: &mut Vec<super::team_rows::TeamLineRef>,
        collapsed: bool,
        now_ms: u64,
    ) -> u64 {
        use super::team_rows::TeamLineRef;
        let visible = |task: &WorkRecent| now_ms < recent_hidden_at(task);
        let expires_at_ms = self
            .recent
            .iter()
            .filter(|task| visible(task))
            .map(recent_hidden_at)
            .min()
            .unwrap_or(u64::MAX);
        if self.tasks.is_empty() && expires_at_ms == u64::MAX {
            return expires_at_ms;
        }
        lines.push(TeamLineRef::WorkHeader { collapsed });
        if collapsed {
            return expires_at_ms;
        }
        lines.extend((0..self.tasks.len()).map(TeamLineRef::Task));
        lines.extend(
            self.recent
                .iter()
                .enumerate()
                .filter(|(_, task)| visible(task))
                .map(|(index, _)| TeamLineRef::Recent(index)),
        );
        expires_at_ms
    }

    pub(super) fn active_count(&self) -> usize {
        self.tasks.len()
    }

    pub(super) fn blocked_count(&self) -> usize {
        self.blocked
    }

    pub(super) fn task(&self, index: usize) -> &WorkTask {
        &self.tasks[index]
    }

    pub(super) fn recent(&self, index: usize) -> &WorkRecent {
        &self.recent[index]
    }
}

/// When a resolved task leaves the board.
fn recent_hidden_at(task: &WorkRecent) -> u64 {
    task.finished_at_ms.saturating_add(RECENT_VISIBLE_MS)
}

impl WorkTask {
    /// The worker's pane, the task row's click target.
    pub(super) fn pane_id(&self) -> Option<&str> {
        self.pane_id.as_deref()
    }
}

/// `elapsed_ms` as `45s`, `3m`, `2h` or `4d`, written into `buffer`.
pub(super) fn age_text(elapsed_ms: u64, buffer: &mut [u8; AGE_BUFFER_LEN]) -> &str {
    let seconds = elapsed_ms / 1000;
    let (value, unit) = match seconds {
        0..=59 => (seconds, b's'),
        60..=3599 => (seconds / 60, b'm'),
        3600..=86399 => (seconds / 3600, b'h'),
        _ => (seconds / 86400, b'd'),
    };
    let mut value = value.min(MAX_AGE_VALUE);
    let mut start = buffer.len() - 1;
    buffer[start] = unit;
    loop {
        start -= 1;
        buffer[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&buffer[start..]).unwrap_or_default()
}

/// Writes `value` in decimal without allocating; returns the next column.
pub(super) fn put_number(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    mut value: usize,
    style: Style,
) -> u16 {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    match std::str::from_utf8(&digits[start..]) {
        Ok(text) => put_segment(buffer, x, y, right, text, style),
        Err(_) => x,
    }
}

pub(super) fn render_work_header(
    buffer: &mut Buffer,
    rect: Rect,
    active: usize,
    blocked: usize,
    collapsed: bool,
    context: &TeamRenderContext<'_>,
) {
    let palette = &context.config.palette;
    let right = rect.right();
    let dim = Style::default().fg(palette.overlay0);
    let glyph = if collapsed {
        COLLAPSED_GLYPH
    } else {
        EXPANDED_GLYPH
    };
    let mut x = put_segment(buffer, rect.x + TASK_INDENT, rect.y, right, glyph, dim);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(
        buffer,
        x,
        rect.y,
        right,
        WORK_TITLE,
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD),
    );
    x = put_segment(buffer, x, rect.y, right, "  ", Style::default());
    x = put_number(buffer, x, rect.y, right, active, dim);
    x = put_segment(buffer, x, rect.y, right, " active · ", dim);
    let blocked_style = if blocked > 0 {
        Style::default().fg(status_color(AgentStatus::Blocked, palette))
    } else {
        dim
    };
    x = put_number(buffer, x, rect.y, right, blocked, blocked_style);
    put_segment(buffer, x, rect.y, right, " blocked", blocked_style);
}

/// Writes `parts` right-aligned on the row, only when they fit after `left`.
fn put_right_parts(buffer: &mut Buffer, rect: Rect, left: u16, parts: [(&str, Style); 3]) {
    let width = parts
        .iter()
        .map(|(text, _)| display_width(text))
        .sum::<u16>();
    let start = rect.right().saturating_sub(width + 1);
    if start <= left {
        return;
    }
    let mut x = start;
    for (text, style) in parts {
        x = put_segment(buffer, x, rect.y, rect.right(), text, style);
    }
}

pub(super) fn render_task(
    buffer: &mut Buffer,
    rect: Rect,
    task: &WorkTask,
    context: &TeamRenderContext<'_>,
) {
    let config = context.config;
    let palette = &config.palette;
    let phase = context.animation.agent_row(
        context.endpoint_id,
        task.pane_id.as_deref().unwrap_or_default(),
        task.status,
    );
    let (icon, icon_fg) = super::agent_sidebar::agent_status_icon_and_color(
        task.status,
        config.status_indicators,
        palette,
        phase,
    );
    if task.status == AgentStatus::Blocked && phase.blocked_pulse > 0.0 {
        let bg = super::animation::blend_color(
            palette.sidebar_bg,
            palette.surface1,
            phase.blocked_pulse * 0.65,
            palette,
        );
        buffer.set_style(rect, Style::default().bg(bg));
    }
    let right = rect.right();
    let dim = Style::default().fg(palette.overlay0);
    let mut x = put_segment(
        buffer,
        rect.x + TASK_INDENT,
        rect.y,
        right,
        icon,
        Style::default().fg(icon_fg),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    if task.awaiting_approval {
        x = put_segment(
            buffer,
            x,
            rect.y,
            right,
            APPROVAL_MARKER,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
    }
    x = put_segment(buffer, x, rect.y, right, &task.short_id, dim);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    let who_style = Style::default().fg(if task.status == AgentStatus::Blocked {
        palette.text
    } else {
        palette.subtext0
    });
    x = put_segment(buffer, x, rect.y, right, &task.who, who_style);
    let mut age = [0u8; AGE_BUFFER_LEN];
    let age = age_text(
        context.now_unix_ms.saturating_sub(task.created_at_ms),
        &mut age,
    );
    put_right_parts(
        buffer,
        rect,
        x,
        [(&task.state, dim), (" ", Style::default()), (age, dim)],
    );
    if rect.height > 1 {
        put_text(
            buffer,
            rect.x + TASK_DETAIL_INDENT,
            rect.y + 1,
            rect.width.saturating_sub(TASK_DETAIL_INDENT),
            &task.title,
            Style::default().fg(palette.overlay1),
        );
    }
}

pub(super) fn render_recent(
    buffer: &mut Buffer,
    rect: Rect,
    task: &WorkRecent,
    context: &TeamRenderContext<'_>,
) {
    let palette = &context.config.palette;
    let right = rect.right();
    let dim = Style::default()
        .fg(palette.overlay0)
        .add_modifier(Modifier::DIM);
    let glyph_fg = match task.outcome.as_str() {
        "completed" => status_color(AgentStatus::Done, palette),
        "failed" | "blocked" => status_color(AgentStatus::Blocked, palette),
        _ => palette.overlay0,
    };
    let mut x = put_segment(
        buffer,
        rect.x + TASK_INDENT,
        rect.y,
        right,
        task.glyph,
        Style::default().fg(glyph_fg),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(buffer, x, rect.y, right, &task.short_id, dim);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(buffer, x, rect.y, right, &task.outcome, dim);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    let mut age = [0u8; AGE_BUFFER_LEN];
    let age = age_text(
        context.now_unix_ms.saturating_sub(task.finished_at_ms),
        &mut age,
    );
    let age_width = display_width(age);
    let summary_width = rect.right().saturating_sub(x).saturating_sub(age_width + 2);
    put_text(
        buffer,
        x,
        rect.y,
        summary_width,
        &task.summary,
        Style::default().fg(palette.overlay1),
    );
    put_text(
        buffer,
        rect.right().saturating_sub(age_width + 1),
        rect.y,
        age_width,
        age,
        dim,
    );
}

/// Maps the frame's monotonic time to Unix milliseconds: one wall-clock reading
/// when the state is created, then `Instant` arithmetic, so a frame never reads
/// the system clock. Also the endpoint's Orbit clock: anchored at the
/// snapshot's `server_now_ms` when it arrived, so ages follow the endpoint's
/// ledger clock however skewed this client's clock is.
#[derive(Clone, Copy, Debug)]
pub(super) struct WallClock {
    anchor: std::time::Instant,
    anchor_unix_ms: u64,
}

impl WallClock {
    pub(super) fn now() -> Self {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        Self::at(std::time::Instant::now(), unix_ms)
    }

    pub(super) fn at(anchor: std::time::Instant, anchor_unix_ms: u64) -> Self {
        Self {
            anchor,
            anchor_unix_ms,
        }
    }

    pub(super) fn unix_ms_at(self, instant: std::time::Instant) -> u64 {
        self.anchor_unix_ms
            .saturating_add(instant.saturating_duration_since(self.anchor).as_millis() as u64)
    }
}
