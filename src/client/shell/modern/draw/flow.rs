//! The Orbit flow view's contents: the lead → role → member steps of one task as
//! a column of steps joined by handoff arrows, parallel steps side by side (or
//! as a list when narrow), the integrator, and a row of task chips. Drawing
//! borrows the snapshot and the precomputed `FlowLayout`; it formats nothing and
//! uses bounded inline hit rectangles without allocating.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::client::shell::modern::model::flow::{FlowHits, FlowLayout, FlowRow};
use crate::client::shell::ClientOrbitFlowOverlay;
use crate::protocol::{OrbitShellActiveTask, OrbitShellStep, OrbitShellView};
use crate::ui::chrome::age::{age_text, AGE_BUFFER_LEN};
use crate::ui::chrome::draw::{display_width, put_elided, put_number, put_segment};
use crate::ui::chrome::state::StateKind;
use crate::ui::chrome::Chrome;

const INDENT: u16 = 1;
/// Title, a blank row, the task chips, the key hints and a blank row.
const CHROME_ROWS: u16 = 5;
const MAX_CHIP_TITLE: u16 = 22;
const EMPTY_TEXT: &str = "no active Orbit tasks";
const HINT_FOCUS: &str = "j/k step  h/l task  enter focus  esc close";
const HINT_NO_PANE: &str = "j/k step  h/l task  no pane for this step  esc close";
const TITLE: &str = " orbit flow";

/// What drawing needs besides the snapshot.
pub(in crate::client::shell) struct FlowDraw<'a> {
    pub(in crate::client::shell) chrome: &'a Chrome,
    pub(in crate::client::shell) spinner: Option<u8>,
    pub(in crate::client::shell) now_unix_ms: u64,
    /// Whether the selected slot's worker pane can be focused.
    pub(in crate::client::shell) selected_pane: bool,
}

/// The state a step shows: approval and blocked belong to the active step that
/// is the task's current worker; a waiting parallel step is queued.
fn step_state(task: &OrbitShellActiveTask, step: &OrbitShellStep) -> StateKind {
    match step.state.as_str() {
        "active" => {
            let current = step.parallel.is_none()
                || step.worker_terminal_id.as_deref() == Some(&task.worker_terminal_id);
            if current && task.awaiting_approval {
                StateKind::Approval
            } else if current && task.blocked {
                StateKind::Blocked
            } else if step.parallel.as_deref() == Some("waiting") {
                StateKind::Queued
            } else {
                StateKind::Working
            }
        }
        "done" | "handed_off" => StateKind::Done,
        "failed" => StateKind::Failed,
        "pending" => StateKind::Queued,
        _ => StateKind::Unknown,
    }
}

fn task_state(task: &OrbitShellActiveTask) -> StateKind {
    if task.awaiting_approval {
        StateKind::Approval
    } else if task.blocked {
        StateKind::Blocked
    } else if task.state == "assigning" {
        StateKind::Queued
    } else {
        StateKind::Working
    }
}

struct Pen<'a> {
    buffer: &'a mut Buffer,
    draw: &'a FlowDraw<'a>,
    inner: Rect,
}

impl Pen<'_> {
    fn glyph(&self, state: StateKind) -> (&'static str, Style) {
        let chrome = self.draw.chrome;
        let glyph = if state == StateKind::Working {
            chrome.glyphs.working_frame(self.draw.spinner)
        } else {
            chrome.glyphs.state(state)
        };
        let mut style = Style::default().fg(chrome.roles.state(state));
        if chrome.roles.bold_states {
            style = style.add_modifier(Modifier::BOLD);
        }
        (glyph, style)
    }

    fn put(&mut self, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
        put_segment(self.buffer, x, y, right, text, style)
    }

    fn elide(&mut self, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
        let ellipsis = self.draw.chrome.glyphs.ellipsis;
        put_elided(self.buffer, x, y, right, text, ellipsis, style)
    }

    fn highlight(&mut self, rect: Rect) {
        let bg = self.draw.chrome.roles.row_focus_bg;
        self.buffer.set_style(rect, Style::default().bg(bg));
    }
}

/// Draws the flow view into `inner` (the popup without its border). Step and
/// task hit rectangles are appended to the lists.
pub(in crate::client::shell) fn render(
    buffer: &mut Buffer,
    inner: Rect,
    flow: &ClientOrbitFlowOverlay,
    orbit: Option<&OrbitShellView>,
    draw: &FlowDraw<'_>,
    step_hits: &mut FlowHits,
    task_hits: &mut FlowHits,
) {
    let roles = &draw.chrome.roles;
    let mut pen = Pen {
        buffer,
        draw,
        inner,
    };
    let right = inner.right();
    pen.put(
        inner.x,
        inner.y,
        right,
        TITLE,
        Style::default()
            .fg(roles.text_strong)
            .add_modifier(Modifier::BOLD),
    );
    let muted = Style::default().fg(roles.text_muted);
    let task = orbit.and_then(|orbit| orbit.active_tasks.get(flow.task));
    let (Some(orbit), Some(task), Some(layout)) = (orbit, task, flow.layout.as_ref()) else {
        pen.put(inner.x + INDENT, inner.y + 2, right, EMPTY_TEXT, muted);
        return;
    };
    // The task's title next to the popup title, clear of the close button.
    let title_x = inner.x + display_width(TITLE) + 2;
    pen.elide(
        title_x,
        inner.y,
        right.saturating_sub(14),
        &task.title,
        Style::default().fg(roles.text_subtle),
    );
    let body_top = inner.y + 2;
    let body_height = inner.height.saturating_sub(CHROME_ROWS);
    let total = layout.height();
    let (selected_y, selected_height) = layout.slot_extent(flow.step).unwrap_or((0, 1));
    let offset = if total > body_height {
        (selected_y + selected_height)
            .saturating_sub(body_height)
            .min(total - body_height)
    } else {
        0
    };
    let mut y = 0u16;
    for row in &layout.rows {
        let height = row.height();
        let top = y.checked_sub(offset);
        y += height;
        let Some(top) = top else { continue };
        if top + height > body_height {
            break;
        }
        let line = body_top + top;
        draw_row(
            &mut pen, *row, line, orbit, task, layout, flow.step, step_hits,
        );
    }
    draw_chips(&mut pen, orbit, flow.task, inner.bottom() - 3, task_hits);
    let hint = if draw.selected_pane {
        HINT_FOCUS
    } else {
        HINT_NO_PANE
    };
    pen.put(inner.x + INDENT, inner.bottom() - 1, right, hint, muted);
}

fn row_rect(inner: Rect, y: u16) -> Rect {
    Rect::new(inner.x, y, inner.width, 1)
}

fn draw_row(
    pen: &mut Pen<'_>,
    row: FlowRow,
    y: u16,
    orbit: &OrbitShellView,
    task: &OrbitShellActiveTask,
    layout: &FlowLayout,
    selected: usize,
    hits: &mut FlowHits,
) {
    let chrome = pen.draw.chrome;
    let roles = &chrome.roles;
    let inner = pen.inner;
    let right = inner.right();
    let x = inner.x + INDENT;
    let muted = Style::default().fg(roles.text_muted);
    match row {
        FlowRow::Header => {
            let (glyph, glyph_style) = pen.glyph(task_state(task));
            let mut x = pen.put(x, y, right, glyph, glyph_style);
            x = pen.put(x, y, right, " ", Style::default());
            x = pen.put(
                x,
                y,
                right,
                &orbit.lead.name,
                Style::default()
                    .fg(roles.text_strong)
                    .add_modifier(Modifier::BOLD),
            );
            let flow = Style::default().fg(roles.flow);
            x = pen.put(x, y, right, " ", Style::default());
            x = pen.put(x, y, right, chrome.glyphs.bar_full, flow);
            x = pen.put(x, y, right, chrome.glyphs.handoff, flow);
            x = pen.put(x, y, right, " ", Style::default());
            let role = task.role.as_deref().unwrap_or("worker");
            let summary_width = display_width(&layout.summary);
            let role_right = right.saturating_sub(summary_width + 2).max(x);
            pen.elide(x, y, role_right, role, Style::default().fg(roles.text_body));
            if summary_width > 0 {
                pen.put(
                    right.saturating_sub(summary_width + 1),
                    y,
                    right,
                    &layout.summary,
                    muted,
                );
            }
        }
        FlowRow::OmittedBefore(count) | FlowRow::OmittedAfter(count) => {
            let x = pen.put(x, y, right, chrome.glyphs.ellipsis, muted);
            let x = pen.put(x, y, right, " ", muted);
            let x = put_number(pen.buffer, x, y, right, usize::from(count), muted);
            let text = if matches!(row, FlowRow::OmittedBefore(_)) {
                " earlier steps"
            } else {
                " later steps"
            };
            pen.put(x, y, right, text, muted);
        }
        FlowRow::Step(slot) | FlowRow::Integrate(slot) | FlowRow::Branch { slot, .. } => {
            let step = &task.steps[layout.slots[slot].step];
            let rect = row_rect(inner, y);
            if slot == selected {
                pen.highlight(rect);
            }
            hits.push((rect, slot));
            let mut x = x;
            match row {
                FlowRow::Branch { last, .. } => {
                    let tree = if last {
                        chrome.glyphs.tree_end
                    } else {
                        chrome.glyphs.tree_mid
                    };
                    x = pen.put(x, y, right, tree, Style::default().fg(roles.parallel));
                    x = pen.put(x, y, right, " ", Style::default());
                }
                FlowRow::Integrate(_) => {
                    x = pen.put(
                        x,
                        y,
                        right,
                        chrome.glyphs.integrate,
                        Style::default().fg(roles.parallel),
                    );
                    x = pen.put(x, y, right, " ", Style::default());
                }
                _ => {}
            }
            draw_step(pen, x, y, right, task, step, slot == selected);
        }
        FlowRow::Arrow(slot) => {
            let step = &task.steps[layout.slots[slot].step];
            let flow = Style::default().fg(roles.flow);
            let mut x = pen.put(x + 1, y, right, chrome.glyphs.tree_end, flow);
            x = pen.put(x, y, right, chrome.glyphs.handoff, flow);
            x = pen.put(x, y, right, " ", Style::default());
            if let Some(next) = step.next.as_deref() {
                pen.elide(x, y, right, next, Style::default().fg(roles.text_subtle));
            }
        }
        FlowRow::ParallelHead => {
            let style = Style::default().fg(roles.parallel);
            let mut x = pen.put(x, y, right, chrome.glyphs.tree_fork, style);
            x = pen.put(x, y, right, " ", Style::default());
            let text = if task.parallel.is_some() {
                &layout.summary
            } else {
                "parallel"
            };
            pen.put(x, y, right, text, style);
        }
        FlowRow::Columns { first, count } => {
            let total = inner.width.saturating_sub(2 * INDENT);
            let column = total / count as u16;
            for offset in 0..count {
                let slot = first + offset;
                let step = &task.steps[layout.slots[slot].step];
                let left = inner.x + INDENT + column * offset as u16;
                let rect = Rect::new(left, y, column.saturating_sub(1).max(1), 2);
                if slot == selected {
                    pen.highlight(rect);
                }
                hits.push((rect, slot));
                let state = step_state(task, step);
                let (glyph, glyph_style) = pen.glyph(state);
                let limit = left + rect.width;
                let mut cx = pen.put(left, y, limit, glyph, glyph_style);
                cx = pen.put(cx, y, limit, " ", Style::default());
                pen.elide(
                    cx,
                    y,
                    limit,
                    &step.member,
                    member_style(roles, slot == selected),
                );
                // The kind and phase, on the line below, stay visible however
                // narrow the column is.
                let mut cx = pen.elide(left + 2, y + 1, limit, &step.kind, muted);
                if let Some(phase) = phase_of(step) {
                    cx = pen.put(cx, y + 1, limit, " ", Style::default());
                    pen.elide(cx, y + 1, limit, phase, muted);
                }
            }
        }
        FlowRow::Lone => {
            let rect = row_rect(inner, y);
            hits.push((rect, 0));
            pen.highlight(rect);
            let (glyph, glyph_style) = pen.glyph(task_state(task));
            let mut x = pen.put(x, y, right, glyph, glyph_style);
            x = pen.put(x, y, right, " ", Style::default());
            let worker = orbit
                .roles
                .iter()
                .flat_map(|role| &role.members)
                .chain(&orbit.scratch)
                .find(|member| member.terminal_id.as_deref() == Some(&task.worker_terminal_id));
            if let Some(worker) = worker {
                x = pen.elide(x, y, right, &worker.name, member_style(roles, true));
                x = pen.put(x, y, right, " ", Style::default());
                pen.elide(x, y, right, &worker.kind, muted);
            } else {
                pen.elide(x, y, right, &task.worker_terminal_id, muted);
            }
        }
    }
}

fn member_style(roles: &crate::ui::chrome::ChromeRoles, selected: bool) -> Style {
    Style::default()
        .fg(if selected {
            roles.focus
        } else {
            roles.text_strong
        })
        .add_modifier(Modifier::BOLD)
}

fn phase_of(step: &OrbitShellStep) -> Option<&str> {
    step.integrate
        .as_deref()
        .or(step.parallel.as_deref())
        .or_else(|| (step.state == "pending").then_some("waiting"))
}

/// `glyph member kind … phase age note`: the identity is drawn first so it is
/// the last thing to be cut, the note last so it is the first.
fn draw_step(
    pen: &mut Pen<'_>,
    x: u16,
    y: u16,
    right: u16,
    task: &OrbitShellActiveTask,
    step: &OrbitShellStep,
    selected: bool,
) {
    let roles = &pen.draw.chrome.roles;
    let muted = Style::default().fg(roles.text_muted);
    let state = step_state(task, step);
    let (glyph, glyph_style) = pen.glyph(state);
    let mut x = pen.put(x, y, right, glyph, glyph_style);
    x = pen.put(x, y, right, " ", Style::default());
    // The age and phase on the right are reserved before the free text.
    let mut age_buffer = [0u8; AGE_BUFFER_LEN];
    let age = step.started_at_ms.map(|started| {
        age_text(
            pen.draw.now_unix_ms.saturating_sub(started),
            &mut age_buffer,
        )
    });
    let phase = phase_of(step);
    let right_width = age.map_or(0, |age| display_width(age) + 1)
        + phase.map_or(0, |phase| display_width(phase) + 1);
    let free_right = right.saturating_sub(right_width + 1).max(x);
    let member_cap = (free_right.saturating_sub(x) * 2 / 5).max(8);
    x = pen.elide(
        x,
        y,
        (x + member_cap).min(free_right),
        &step.member,
        member_style(roles, selected),
    );
    x = pen.put(x, y, free_right, " ", Style::default());
    x = pen.elide(x, y, free_right, &step.kind, muted);
    if let Some(note) = step.note.as_deref() {
        x = pen.put(x, y, free_right, "  ", Style::default());
        pen.elide(
            x,
            y,
            free_right,
            note,
            Style::default().fg(roles.text_subtle),
        );
    }
    let mut rx = right.saturating_sub(right_width);
    if let Some(phase) = phase {
        rx = pen.put(rx, y, right, phase, muted);
        rx = pen.put(rx, y, right, " ", muted);
    }
    if let Some(age) = age {
        pen.put(rx, y, right, age, muted);
    }
}

/// `tasks ▸ [title ◐]  title ⚑ …`, scrolled so the selected chip is visible.
fn draw_chips(
    pen: &mut Pen<'_>,
    orbit: &OrbitShellView,
    selected: usize,
    y: u16,
    hits: &mut FlowHits,
) {
    let roles = &pen.draw.chrome.roles;
    let inner = pen.inner;
    let right = inner.right();
    let muted = Style::default().fg(roles.text_muted);
    let mut x = pen.put(inner.x + INDENT, y, right, "tasks ", muted);
    let chip_width =
        |task: &OrbitShellActiveTask| display_width(&task.title).min(MAX_CHIP_TITLE) + 5;
    // Skip leading chips until the selected one fits.
    let mut start = 0;
    let mut used: u16 = orbit
        .active_tasks
        .iter()
        .take(selected + 1)
        .map(chip_width)
        .sum();
    while start < selected && x + used > right {
        used = used.saturating_sub(chip_width(&orbit.active_tasks[start]));
        start += 1;
    }
    for (index, task) in orbit.active_tasks.iter().enumerate().skip(start) {
        let width = chip_width(task);
        if x + width > right {
            pen.put(
                right.saturating_sub(1),
                y,
                right,
                pen.draw.chrome.glyphs.ellipsis,
                muted,
            );
            break;
        }
        let chosen = index == selected;
        let (glyph, glyph_style) = pen.glyph(task_state(task));
        let start_x = x;
        let bracket = Style::default().fg(roles.focus);
        x = pen.put(x, y, right, if chosen { "[" } else { " " }, bracket);
        x = pen.elide(
            x,
            y,
            x + MAX_CHIP_TITLE,
            &task.title,
            if chosen {
                member_style(roles, true)
            } else {
                Style::default().fg(roles.text_body)
            },
        );
        x = pen.put(x, y, right, " ", Style::default());
        x = pen.put(x, y, right, glyph, glyph_style);
        x = pen.put(x, y, right, if chosen { "]" } else { " " }, bracket);
        x = pen.put(x, y, right, " ", Style::default());
        hits.push((Rect::new(start_x, y, x - start_x, 1), index));
    }
}
