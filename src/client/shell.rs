use std::collections::{HashMap, HashSet, VecDeque};

mod actions;
mod agent_sidebar;
mod aggregate_navigation;
pub(crate) mod animation;
mod machine_diagnostics;
mod modern;
mod workspace_navigation;
use workspace_navigation::{PendingWorkspaceHighlight, WorkspaceNavigationTarget};
mod composition;
mod config;
mod context_menu;
mod copy_mode;
mod endpoint_agent_state;
mod endpoint_agents;
mod endpoint_navigation;
mod endpoint_notices;
mod endpoint_sidebar;
mod endpoints;
mod flow_view;
pub(super) use endpoints::*;
mod global_menu;
mod graphics;
mod input;
mod input_source;
mod link_hover;
mod mobile;
mod modern_toasts;
mod mouse;
mod notification_policy;
mod notifications;
mod overlay_input;
mod preferences;
mod render;
mod scroll;
mod settings;
mod state;
mod surface_patch;
mod team_rows;
mod team_sidebar;
mod text_editor;
mod word_selection;
mod work_board;
mod worktrees;
use text_editor::TextEditor;
use word_selection::ClientWordSelection;

pub(in crate::client::shell) use render::sidebar;
pub(crate) use state::*;
#[cfg(test)]
pub(super) use surface_patch::apply_composed_surface_patch;
pub(super) use surface_patch::{ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome};

use crossterm::event::KeyCode;
#[cfg(test)]
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::endpoint::{ClientEndpointId, ClientEndpointStatus, SavedSshEndpoint};
use crate::app::state::Palette;
use crate::config::{
    Config, LiveKeybindConfig, SidebarCollapsedModeConfig, SpacesSidebarConfig,
    TabBarPositionConfig,
};
use crate::protocol::{
    ClientMessage, ClientMousePosition, ClientPaneInputEvent, ClientShellSnapshot, ClientShellTab,
    ClientShellWorkspace, ClientSurfaceSize, FrameData, PaneSurfaceFrame, SemanticNotification,
    SemanticNotificationKind, SemanticNotificationSound,
};
#[cfg(test)]
use crate::raw_input::RawInputEvent;

fn target_event_message(target: ClientInputTarget, event: ClientPaneInputEvent) -> ClientMessage {
    match target {
        ClientInputTarget::Pane(pane_id) => ClientMessage::ClientShellPaneInput {
            pane_id,
            events: vec![event],
        },
        ClientInputTarget::Popup(terminal_id) => ClientMessage::ClientShellPopupInput {
            terminal_id,
            events: vec![event],
        },
    }
}

fn push_target_event(
    target: ClientInputTarget,
    event: ClientPaneInputEvent,
    outcome: &mut ClientShellInput,
) {
    match target {
        ClientInputTarget::Pane(pane_id) => {
            if let Some(ClientMessage::ClientShellPaneInput {
                pane_id: pending_pane,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_pane == pane_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Pane(pane_id),
                event,
            ));
        }
        ClientInputTarget::Popup(terminal_id) => {
            if let Some(ClientMessage::ClientShellPopupInput {
                terminal_id: pending_terminal,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_terminal == terminal_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Popup(terminal_id),
                event,
            ));
        }
    }
}

fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.width > 0
        && rect.height > 0
        && point.0 >= rect.x
        && point.0 < rect.right()
        && point.1 >= rect.y
        && point.1 < rect.bottom()
}

/// Only the identities, split paths and directions feed the resize digest.
/// Output, scroll and split positions do not change that identity contract.
fn same_pane_surface_topology(a: &PaneSurfaceFrame, b: &PaneSurfaceFrame) -> bool {
    a.panes.len() == b.panes.len()
        && a.splits.len() == b.splits.len()
        && a.panes
            .iter()
            .zip(&b.panes)
            .all(|(a, b)| a.pane_id == b.pane_id)
        && a.splits
            .iter()
            .zip(&b.splits)
            .all(|(a, b)| a.direction == b.direction && a.path == b.path)
}

fn pane_surface_topology_signature(surface: &PaneSurfaceFrame) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(PRIME);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(PRIME);
    }

    let mut pane_ids = surface
        .panes
        .iter()
        .map(|pane| pane.pane_id.as_bytes())
        .collect::<Vec<_>>();
    pane_ids.sort_unstable();
    let mut hash = OFFSET;
    for pane_id in pane_ids {
        write(&mut hash, pane_id);
    }
    let mut splits = surface.splits.iter().collect::<Vec<_>>();
    splits.sort_by(|left, right| left.path.cmp(&right.path));
    for split in splits {
        write(
            &mut hash,
            &[match split.direction {
                crate::protocol::PaneSurfaceSplitDirection::Horizontal => 0,
                crate::protocol::PaneSurfaceSplitDirection::Vertical => 1,
            }],
        );
        write(
            &mut hash,
            &split
                .path
                .iter()
                .map(|right| u8::from(*right))
                .collect::<Vec<_>>(),
        );
    }
    hash
}

fn status_icon(
    status: crate::api::schema::AgentStatus,
    style: crate::config::StatusIndicatorStyle,
) -> &'static str {
    use crate::api::schema::AgentStatus;
    use crate::config::StatusIndicatorStyle;
    match (style, status) {
        (
            StatusIndicatorStyle::Dots,
            AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done,
        ) => "●",
        (StatusIndicatorStyle::Dots, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Dots, AgentStatus::Unknown) => "·",
        (StatusIndicatorStyle::Symbols, AgentStatus::Blocked) => "×",
        (StatusIndicatorStyle::Symbols, AgentStatus::Working) => "◐",
        (StatusIndicatorStyle::Symbols, AgentStatus::Done) => "✓",
        (StatusIndicatorStyle::Symbols, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Symbols, AgentStatus::Unknown) => "·",
    }
}

fn status_dot(status: crate::api::schema::AgentStatus) -> &'static str {
    status_icon(status, crate::config::StatusIndicatorStyle::Dots)
}

fn status_priority(status: crate::api::schema::AgentStatus) -> u8 {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Done => 3,
        AgentStatus::Working => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

fn status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

fn status_color(
    status: crate::api::schema::AgentStatus,
    palette: &Palette,
) -> ratatui::style::Color {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => palette.yellow,
        AgentStatus::Blocked => palette.red,
        AgentStatus::Done => palette.teal,
        AgentStatus::Idle => palette.green,
        AgentStatus::Unknown => palette.overlay0,
    }
}

fn panel_contrast_fg(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}

fn blit_pane_surface(target: &mut FrameData, source: &FrameData, area: Rect) {
    blit_pane_surface_cells(target, source, area, |cell, _, _| cell.clone());
}

/// Uses the existing cell-copy allocation only for cached visible header spans.
/// The ordered span cursor advances once per span; no frame scratch allocation
/// or row-by-pane scan is needed.
#[cfg(test)]
fn blit_pane_surface_with_headers(
    target: &mut FrameData,
    source: &FrameData,
    area: Rect,
    spans: &[modern::model::pane_header::HeaderCopy],
) {
    let mut cursor = 0;
    blit_pane_surface_cells(target, source, area, |cell, x, y| {
        let x = x.saturating_sub(area.x);
        let y = y.saturating_sub(area.y);
        while spans
            .get(cursor)
            .is_some_and(|span| span.row < y || (span.row == y && span.to <= x))
        {
            cursor += 1;
        }
        let capacity = spans
            .get(cursor)
            .filter(|span| span.row == y && span.from <= x && x < span.to)
            .map_or(0, |span| span.bytes);
        if capacity == 0 {
            return cell.clone();
        }
        let mut symbol = String::with_capacity(cell.symbol.len().max(capacity));
        symbol.push_str(&cell.symbol);
        crate::protocol::CellData {
            symbol,
            fg: cell.fg,
            bg: cell.bg,
            modifier: cell.modifier,
            skip: cell.skip,
            hyperlink: cell.hyperlink,
        }
    });
}

/// Copy ordinary rows directly. Header rows reuse that copy to reserve symbols,
/// discover junction stops and clear residue before the allocation-free painter.
fn blit_pane_surface_modern(
    target: &mut FrameData,
    surface: &crate::protocol::PaneSurfaceFrame,
    area: Rect,
    spans: &[modern::model::pane_header::HeaderCopy],
    team: &team_sidebar::TeamSidebar,
    ctx: &team_sidebar::TeamRenderContext<'_>,
) {
    use modern::draw::pane_header::{is_stop, paint_prepared, prepare};
    let source = &surface.frame;
    let copy_width = source.width.min(area.width);
    let copy_height = source.height.min(area.height);
    let hyperlink_base = target.hyperlinks.len() as u32;
    target.hyperlinks.extend(source.hyperlinks.iter().cloned());
    let mut cursor = 0;
    for row in 0..copy_height {
        let start = usize::from(row) * usize::from(source.width);
        let Some(source_row) = source.cells.get(start..start + usize::from(copy_width)) else {
            continue;
        };
        let global_y = area.y + row;
        let start = usize::from(global_y) * usize::from(target.width);
        let Some(target_row) = target
            .cells
            .get_mut(start..start + usize::from(target.width))
        else {
            continue;
        };
        let max = copy_width.min(target.width.saturating_sub(area.x));
        if max == 0 {
            continue;
        }
        while spans.get(cursor).is_some_and(|span| span.row < row) {
            cursor += 1;
        }
        let mut col = 0;
        while let Some(span) = spans.get(cursor).filter(|span| span.row == row) {
            cursor += 1;
            let from = span.from.min(max);
            let to = span.to.min(max);
            if from < col || to <= from {
                continue;
            }
            copy_plain_cells(
                &mut target_row[usize::from(area.x + col)..usize::from(area.x + from)],
                &source_row[usize::from(col)..usize::from(from)],
                hyperlink_base,
                source.hyperlinks.len(),
            );
            let header = surface.panes.get(span.pane).and_then(|pane| {
                team.modern
                    .headers
                    .get(&pane.pane_id)
                    .map(|header| (pane, header))
            });
            let Some((pane, header)) = header else {
                copy_plain_cells(
                    &mut target_row[usize::from(area.x + from)..usize::from(area.x + to)],
                    &source_row[usize::from(from)..usize::from(to)],
                    hyperlink_base,
                    source.hyperlinks.len(),
                );
                col = to;
                continue;
            };
            let prepared = prepare(header, &pane.pane_id, ctx);
            let border = &source_row[usize::from(from.saturating_sub(1))];
            let mut end = to;
            let mut clear = true;
            for x in from..to {
                let source_cell = &source_row[usize::from(x)];
                let target_cell = &mut target_row[usize::from(area.x + x)];
                if clear && is_stop(&source_cell.symbol) {
                    clear = false;
                    end = x;
                }
                if clear {
                    target_cell
                        .symbol
                        .reserve(span.bytes.saturating_sub(target_cell.symbol.len()));
                    target_cell.symbol.clear();
                    target_cell.symbol.push('─');
                    target_cell.fg = border.fg;
                    target_cell.bg = prepared.background().unwrap_or(border.bg);
                    target_cell.modifier = border.modifier;
                    target_cell.hyperlink = None;
                    target_cell.skip = false;
                } else {
                    copy_plain_cells(
                        std::slice::from_mut(target_cell),
                        std::slice::from_ref(source_cell),
                        hyperlink_base,
                        source.hyperlinks.len(),
                    );
                }
            }
            paint_prepared(
                target_row,
                usize::from(area.x + from),
                usize::from(area.x + end),
                header,
                ctx,
                prepared,
            );
            col = to;
        }
        copy_plain_cells(
            &mut target_row[usize::from(area.x + col)..usize::from(area.x + max)],
            &source_row[usize::from(col)..usize::from(max)],
            hyperlink_base,
            source.hyperlinks.len(),
        );
    }
    finish_surface_blit(target, source, area, copy_width, copy_height);
}

/// Copy a popup below the flow view without shrinking reserved output cells.
fn blit_pane_surface_reusing(target: &mut FrameData, source: &FrameData, area: Rect) {
    let width = source
        .width
        .min(area.width)
        .min(target.width.saturating_sub(area.x));
    let height = source
        .height
        .min(area.height)
        .min(target.height.saturating_sub(area.y));
    let base = target.hyperlinks.len() as u32;
    let links = source.hyperlinks.len();
    target.hyperlinks.extend(source.hyperlinks.iter().cloned());
    for row in 0..height {
        let start = usize::from(row) * usize::from(source.width);
        let output = usize::from(area.y + row) * usize::from(target.width) + usize::from(area.x);
        let len = usize::from(width);
        if let (Some(source), Some(target)) = (
            source.cells.get(start..start + len),
            target.cells.get_mut(output..output + len),
        ) {
            copy_plain_cells(target, source, base, links);
        }
    }
    finish_surface_blit(target, source, area, width, height);
}

fn copy_plain_cells(
    target: &mut [crate::protocol::CellData],
    source: &[crate::protocol::CellData],
    hyperlink_base: u32,
    hyperlink_count: usize,
) {
    for (target, source) in target.iter_mut().zip(source) {
        target.symbol.clone_from(&source.symbol);
        target.fg = source.fg;
        target.bg = source.bg;
        target.modifier = source.modifier;
        target.skip = source.skip;
        target.hyperlink = source
            .hyperlink
            .filter(|&index| (index as usize) < hyperlink_count)
            .map(|index| hyperlink_base + index);
    }
}

fn finish_surface_blit(
    target: &mut FrameData,
    source: &FrameData,
    area: Rect,
    copy_width: u16,
    copy_height: u16,
) {
    target.cursor = source.cursor.as_ref().and_then(|cursor| {
        (cursor.x < copy_width && cursor.y < copy_height).then(|| crate::protocol::CursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    });
    target.graphics.clear();
}

fn blit_pane_surface_cells(
    target: &mut FrameData,
    source: &FrameData,
    area: Rect,
    mut copy: impl FnMut(&crate::protocol::CellData, u16, u16) -> crate::protocol::CellData,
) {
    let copy_width = source.width.min(area.width);
    let copy_height = source.height.min(area.height);
    let hyperlink_base = target.hyperlinks.len() as u32;
    target.hyperlinks.extend(source.hyperlinks.iter().cloned());

    for row in 0..copy_height {
        for col in 0..copy_width {
            let source_index = row as usize * source.width as usize + col as usize;
            let target_x = area.x + col;
            let target_y = area.y + row;
            let target_index = target_y as usize * target.width as usize + target_x as usize;
            let (Some(source_cell), Some(target_cell)) = (
                source.cells.get(source_index),
                target.cells.get_mut(target_index),
            ) else {
                continue;
            };
            *target_cell = copy(source_cell, target_x, target_y);
            target_cell.hyperlink = source_cell.hyperlink.and_then(|index| {
                ((index as usize) < source.hyperlinks.len()).then_some(hyperlink_base + index)
            });
        }
    }

    target.cursor = source.cursor.as_ref().and_then(|cursor| {
        (cursor.x < copy_width && cursor.y < copy_height).then(|| crate::protocol::CursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    });
    target.graphics.clear();
}

#[cfg(test)]
mod tests;
